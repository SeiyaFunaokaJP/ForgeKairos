//! TeraTerm-style session tooling: quick connect, the macro picker and the
//! macro editor.
//!
//! The data these modals read and write lives in `terminal_sessions.json`
//! (see [`terminal::terminal_macro`]); this module is only the UI over it.

use std::sync::Arc;

use editor::Editor;
use gpui::{
    App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Task, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::Fs;
use settings::Settings as _;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal::{
    Terminal,
    terminal_macro::{
        ConnectionProtocol, MacroStep, SavedConnection, TerminalMacro, TerminalSessionStore,
    },
    terminal_settings::TerminalSettings,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, ListItem, ListItemSpacing, Modal,
    ModalFooter, ModalHeader, Section, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Panel as _, Workspace};

use crate::{TerminalPanel, TerminalView};

actions!(
    terminal,
    [
        /// 保存済みのマクロを選んで、アクティブなターミナルで実行します。
        RunMacro,
        /// マクロの一覧を編集します。
        EditMacros,
        /// SSH / Telnet の接続先を指定してターミナルを開きます。
        QuickConnect,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &RunMacro, window, cx| {
            let Some(fs) = workspace_fs(workspace) else {
                return;
            };
            let store = TerminalSessionStore::global(fs, cx);
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                MacroPicker::new(store, workspace_handle, window, cx)
            });
        });
        workspace.register_action(|workspace, _: &EditMacros, window, cx| {
            let Some(fs) = workspace_fs(workspace) else {
                return;
            };
            let store = TerminalSessionStore::global(fs, cx);
            workspace.toggle_modal(window, cx, |window, cx| {
                MacroEditor::new(store, None, window, cx)
            });
        });
        workspace.register_action(|workspace, _: &QuickConnect, window, cx| {
            let Some(fs) = workspace_fs(workspace) else {
                return;
            };
            let store = TerminalSessionStore::global(fs, cx);
            let workspace_handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                QuickConnectModal::new(store, workspace_handle, window, cx)
            });
        });
    })
    .detach();
}

fn workspace_fs(workspace: &Workspace) -> Option<Arc<dyn Fs>> {
    Some(workspace.app_state().fs.clone())
}

/// The terminal the session tools act on: the focused one when the terminal
/// panel has focus, otherwise the active center item.
pub fn active_terminal_view(
    workspace: &Workspace,
    window: &Window,
    cx: &App,
) -> Option<Entity<TerminalView>> {
    if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx)
        && let Some(pane) = terminal_panel.read(cx).pane()
        && let Some(view) = pane
            .read(cx)
            .active_item()
            .and_then(|item| item.downcast::<TerminalView>())
    {
        if terminal_panel
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            return Some(view);
        }
    }

    if let Some(view) = workspace
        .active_pane()
        .read(cx)
        .active_item()
        .and_then(|item| item.downcast::<TerminalView>())
    {
        return Some(view);
    }

    workspace
        .panel::<TerminalPanel>(cx)
        .and_then(|terminal_panel| terminal_panel.read(cx).pane())
        .and_then(|pane| {
            pane.read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
        })
}

fn run_macro_on_active_terminal(
    workspace: &Workspace,
    terminal_macro: TerminalMacro,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), &'static str> {
    let Some(view) = active_terminal_view(workspace, window, cx) else {
        return Err("マクロを実行するターミナルがありません。ターミナルを開いてください。");
    };
    view.update(cx, |view, cx| view.run_macro(terminal_macro, cx));
    Ok(())
}

// -------------------------------------------------------------------------
// Macro picker
// -------------------------------------------------------------------------

pub struct MacroPicker {
    picker: Entity<Picker<MacroPickerDelegate>>,
}

impl MacroPicker {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let macros = store.read(cx).macros().to_vec();
        let delegate = MacroPickerDelegate {
            store,
            workspace,
            matches: (0..macros.len()).collect(),
            macros,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for MacroPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(rems(34.))
            .key_context("MacroPicker")
            .child(self.picker.clone())
    }
}

impl EventEmitter<DismissEvent> for MacroPicker {}

impl Focusable for MacroPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.read(cx).focus_handle(cx)
    }
}

impl ModalView for MacroPicker {}

pub struct MacroPickerDelegate {
    store: Entity<TerminalSessionStore>,
    workspace: WeakEntity<Workspace>,
    macros: Vec<TerminalMacro>,
    matches: Vec<usize>,
    selected_index: usize,
}

impl MacroPickerDelegate {
    fn selected_macro(&self) -> Option<&TerminalMacro> {
        let index = *self.matches.get(self.selected_index)?;
        self.macros.get(index)
    }

    fn open_editor(
        &self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let store = self.store.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    MacroEditor::new(store, edited, window, cx)
                });
            })
            .log_err();
    }
}

impl PickerDelegate for MacroPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "TerminalMacroPicker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "マクロ名で絞り込み…".into()
    }

    fn no_matches_text(&self, _: &mut Window, _: &mut App) -> Option<SharedString> {
        Some("保存済みのマクロがありません".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.macros = self.store.read(cx).macros().to_vec();
        let query = query.trim().to_lowercase();
        self.matches = self
            .macros
            .iter()
            .enumerate()
            .filter(|(_, terminal_macro)| {
                query.is_empty() || terminal_macro.name.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(terminal_macro) = self.selected_macro().cloned() else {
            return;
        };
        if secondary {
            self.open_editor(Some(terminal_macro), window, cx);
            return;
        }

        let outcome = self.workspace.update(cx, |workspace, cx| {
            run_macro_on_active_terminal(workspace, terminal_macro, window, cx)
        });
        match outcome {
            Ok(Err(message)) => {
                self.workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error(message.to_string(), cx)
                    })
                    .log_err();
            }
            Ok(Ok(())) | Err(_) => {}
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let terminal_macro = self.macros.get(*self.matches.get(ix)?)?;
        let description = if terminal_macro.description.is_empty() {
            format!("{} ステップ", terminal_macro.steps.len())
        } else {
            format!(
                "{} ({} ステップ)",
                terminal_macro.description,
                terminal_macro.steps.len()
            )
        };
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(Label::new(terminal_macro.name.clone()))
                        .child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                ),
        )
    }

    fn render_footer(&self, _: &mut Window, cx: &mut Context<Picker<Self>>) -> Option<AnyElement> {
        Some(
            h_flex()
                .w_full()
                .p_1()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Button::new("edit-macro", "編集").on_click(cx.listener(
                    |picker, _, window, cx| {
                        let edited = picker.delegate.selected_macro().cloned();
                        if edited.is_some() {
                            picker.delegate.open_editor(edited, window, cx);
                        }
                    },
                )))
                .child(Button::new("new-macro", "新規作成").on_click(cx.listener(
                    |picker, _, window, cx| {
                        picker.delegate.open_editor(None, window, cx);
                    },
                )))
                .into_any_element(),
        )
    }
}

// -------------------------------------------------------------------------
// Macro editor
// -------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Send,
    WaitFor,
    Sleep,
}

impl StepKind {
    fn label(&self) -> &'static str {
        match self {
            StepKind::Send => "送信",
            StepKind::WaitFor => "期待文字を待つ",
            StepKind::Sleep => "秒待ち",
        }
    }

    const ALL: [StepKind; 3] = [StepKind::Send, StepKind::WaitFor, StepKind::Sleep];
}

/// One editable row. The editors travel with the row when it is reordered, so
/// a moved row never picks up the value of the row that took its place.
struct StepRow {
    kind: StepKind,
    text: Entity<Editor>,
    number: Entity<Editor>,
    regex: bool,
    newline: bool,
}

impl StepRow {
    fn new(step: &MacroStep, window: &mut Window, cx: &mut App) -> Self {
        let (kind, text_value, number_value, regex, newline) = match step {
            MacroStep::Send { text, newline } => {
                (StepKind::Send, text.clone(), String::new(), false, *newline)
            }
            MacroStep::WaitFor {
                pattern,
                regex,
                timeout_ms,
            } => (
                StepKind::WaitFor,
                pattern.clone(),
                format_seconds(*timeout_ms),
                *regex,
                true,
            ),
            MacroStep::Sleep { ms } => (
                StepKind::Sleep,
                String::new(),
                format_seconds(*ms),
                false,
                true,
            ),
        };

        let text = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(text_value, window, cx);
            editor
        });
        let number = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(number_value, window, cx);
            editor
        });

        Self {
            kind,
            text,
            number,
            regex,
            newline,
        }
    }

    fn to_step(&self, index: usize, cx: &App) -> Result<MacroStep, String> {
        let text = self.text.read(cx).text(cx);
        match self.kind {
            StepKind::Send => Ok(MacroStep::Send {
                text,
                newline: self.newline,
            }),
            StepKind::WaitFor => {
                if text.trim().is_empty() {
                    return Err(format!(
                        "ステップ {}: 期待文字を入力してください。",
                        index + 1
                    ));
                }
                if self.regex && regex::Regex::new(&text).is_err() {
                    return Err(format!("ステップ {}: 正規表現が不正です。", index + 1));
                }
                Ok(MacroStep::WaitFor {
                    pattern: text,
                    regex: self.regex,
                    timeout_ms: parse_seconds(&self.number.read(cx).text(cx))
                        .ok_or_else(|| format!("ステップ {}: 秒数が不正です。", index + 1))?,
                })
            }
            StepKind::Sleep => Ok(MacroStep::Sleep {
                ms: parse_seconds(&self.number.read(cx).text(cx))
                    .ok_or_else(|| format!("ステップ {}: 秒数が不正です。", index + 1))?,
            }),
        }
    }
}

fn format_seconds(milliseconds: u64) -> String {
    let seconds = milliseconds as f64 / 1000.0;
    if (seconds.fract()).abs() < f64::EPSILON {
        format!("{}", seconds as u64)
    } else {
        format!("{seconds}")
    }
}

fn parse_seconds(value: &str) -> Option<u64> {
    let seconds: f64 = value.trim().parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1000.0).round() as u64)
}

pub struct MacroEditor {
    store: Entity<TerminalSessionStore>,
    /// Name the macro had when the editor opened, so a rename replaces the
    /// original entry instead of adding a second one.
    original_name: Option<String>,
    name: Entity<Editor>,
    description: Entity<Editor>,
    steps: Vec<StepRow>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl MacroEditor {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let edited = edited.unwrap_or_default();
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("マクロ名", window, cx);
            editor.set_text(edited.name.clone(), window, cx);
            editor
        });
        let description = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("説明 (省略可)", window, cx);
            editor.set_text(edited.description.clone(), window, cx);
            editor
        });
        let steps = edited
            .steps
            .iter()
            .map(|step| StepRow::new(step, window, cx))
            .collect();

        let focus_handle = cx.focus_handle();
        window.focus(&name.focus_handle(cx), cx);

        Self {
            store,
            original_name: (!edited.name.is_empty()).then_some(edited.name),
            name,
            description,
            steps,
            error: None,
            focus_handle,
        }
    }

    fn add_step(&mut self, kind: StepKind, window: &mut Window, cx: &mut Context<Self>) {
        let step = match kind {
            StepKind::Send => MacroStep::Send {
                text: String::new(),
                newline: true,
            },
            StepKind::WaitFor => MacroStep::WaitFor {
                pattern: String::new(),
                regex: false,
                timeout_ms: terminal::terminal_macro::DEFAULT_WAIT_TIMEOUT_MS,
            },
            StepKind::Sleep => MacroStep::Sleep { ms: 1000 },
        };
        self.steps.push(StepRow::new(&step, window, cx));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.error = Some("マクロ名を入力してください。".into());
            cx.notify();
            return;
        }

        let mut steps = Vec::with_capacity(self.steps.len());
        for (index, row) in self.steps.iter().enumerate() {
            match row.to_step(index, cx) {
                Ok(step) => steps.push(step),
                Err(message) => {
                    self.error = Some(message.into());
                    cx.notify();
                    return;
                }
            }
        }

        let terminal_macro = TerminalMacro {
            name: name.clone(),
            description: self.description.read(cx).text(cx).trim().to_string(),
            steps,
        };

        let original_name = self.original_name.clone();
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            let existing = file
                .macros
                .iter()
                .position(|candidate| Some(&candidate.name) == original_name.as_ref())
                .or_else(|| {
                    file.macros
                        .iter()
                        .position(|candidate| candidate.name == name)
                });
            match existing {
                Some(index) => file.macros[index] = terminal_macro,
                None => file.macros.push(terminal_macro),
            }
            store.write(file, cx)
        });

        cx.spawn(async move |this, cx| match write.await {
            Ok(()) => {
                this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            }
            Err(error) => {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("保存できませんでした: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn delete_macro(&mut self, cx: &mut Context<Self>) {
        let Some(original_name) = self.original_name.clone() else {
            cx.emit(DismissEvent);
            return;
        };
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            file.macros
                .retain(|candidate| candidate.name != original_name);
            store.write(file, cx)
        });
        cx.spawn(async move |this, cx| {
            write.await.log_err();
            this.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        })
        .detach();
    }

    fn render_step(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row = &self.steps[index];
        let kind = row.kind;

        let step_count = self.steps.len();

        let view = cx.weak_entity();
        let kind_menu = ContextMenu::build(window, cx, {
            move |mut menu, _, _| {
                for candidate in StepKind::ALL {
                    let view = view.clone();
                    menu = menu.toggleable_entry(
                        candidate.label(),
                        candidate == kind,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            view.update(cx, |this, cx| {
                                let Some(row) = this.steps.get_mut(index) else {
                                    return;
                                };
                                row.kind = candidate;
                                // Switching into a step that needs a duration
                                // would otherwise leave the field blank and
                                // fail validation on save.
                                if candidate != StepKind::Send
                                    && row.number.read(cx).text(cx).trim().is_empty()
                                {
                                    let seconds = match candidate {
                                        StepKind::WaitFor => format_seconds(
                                            terminal::terminal_macro::DEFAULT_WAIT_TIMEOUT_MS,
                                        ),
                                        _ => format_seconds(1_000),
                                    };
                                    row.number.update(cx, |editor, cx| {
                                        editor.set_text(seconds, window, cx)
                                    });
                                }
                                cx.notify();
                            })
                            .ok();
                        },
                    );
                }
                menu
            }
        });

        v_flex()
            .w_full()
            .gap_1()
            .p_1p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(format!("{}", index + 1))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                DropdownMenu::new(("step-kind", index), kind.label(), kind_menu)
                                    .style(DropdownStyle::Outlined),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new(("step-up", index), IconName::ArrowUp)
                                    .icon_size(IconSize::Small)
                                    .disabled(index == 0)
                                    .tooltip(Tooltip::text("ひとつ上へ移動します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if index > 0 && index < this.steps.len() {
                                            this.steps.swap(index, index - 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                IconButton::new(("step-down", index), IconName::ArrowDown)
                                    .icon_size(IconSize::Small)
                                    .disabled(index + 1 >= step_count)
                                    .tooltip(Tooltip::text("ひとつ下へ移動します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if index + 1 < this.steps.len() {
                                            this.steps.swap(index, index + 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                IconButton::new(("step-delete", index), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("このステップを削除します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if index < this.steps.len() {
                                            this.steps.remove(index);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .when(kind != StepKind::Sleep, |this| this.child(row.text.clone()))
            .when(kind == StepKind::Send, |this| {
                this.child(
                    Checkbox::new(("step-newline", index), row.newline.into())
                        .label("末尾で Enter を送る")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                            let checked = *state == ToggleState::Selected;
                            if let Some(row) = this.steps.get_mut(index)
                                && row.newline != checked
                            {
                                row.newline = checked;
                                cx.notify();
                            }
                        })),
                )
            })
            .when(kind != StepKind::Send, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(if kind == StepKind::WaitFor {
                                "タイムアウト (秒)"
                            } else {
                                "秒数"
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                        .child(div().w(rems(6.)).child(row.number.clone()))
                        .when(kind == StepKind::WaitFor, |this| {
                            this.child(
                                Checkbox::new(("step-regex", index), row.regex.into())
                                    .label("正規表現")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(
                                        move |this, state: &ToggleState, _, cx| {
                                            let checked = *state == ToggleState::Selected;
                                            if let Some(row) = this.steps.get_mut(index)
                                                && row.regex != checked
                                            {
                                                row.regex = checked;
                                                cx.notify();
                                            }
                                        },
                                    )),
                            )
                        }),
                )
            })
    }
}

impl Render for MacroEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steps = (0..self.steps.len())
            .map(|index| self.render_step(index, window, cx).into_any_element())
            .collect::<Vec<_>>();
        let can_delete = self.original_name.is_some();

        v_flex()
            .w(rems(40.))
            .elevation_3(cx)
            .key_context("TerminalMacroEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                Modal::new("terminal-macro-editor", None)
                    .header(ModalHeader::new().headline("マクロの編集"))
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(self.name.clone())
                                .child(self.description.clone())
                                .children(steps)
                                .child(
                                    h_flex().gap_1().child(
                                        Button::new("add-step", "ステップを追加")
                                            .start_icon(Icon::new(IconName::Plus))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.add_step(StepKind::Send, window, cx);
                                            })),
                                    ),
                                )
                                .when_some(self.error.clone(), |this, error| {
                                    this.child(Label::new(error).color(Color::Error))
                                }),
                        ),
                    )
                    .footer(
                        ModalFooter::new()
                            .when(can_delete, |footer| {
                                footer.start_slot(
                                    Button::new("delete-macro", "削除")
                                        .color(Color::Error)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.delete_macro(cx);
                                        })),
                                )
                            })
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("cancel", "キャンセル").on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                        ),
                                    )
                                    .child(Button::new("save", "保存").on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.save(cx);
                                        },
                                    ))),
                            ),
                    ),
            )
    }
}

impl EventEmitter<DismissEvent> for MacroEditor {}

impl Focusable for MacroEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MacroEditor {}

// -------------------------------------------------------------------------
// Quick connect
// -------------------------------------------------------------------------

/// Rejects anything that the shell would treat as more than one word, because
/// the spawned command line is assembled without quoting.
fn validate_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("ホスト名または IP アドレスを入力してください。");
    }
    if host.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '[' | ']')
    }) {
        Ok(())
    } else {
        Err("ホスト名に使用できない文字が含まれています。")
    }
}

fn validate_user(user: &str) -> Result<(), &'static str> {
    if user
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'))
    {
        Ok(())
    } else {
        Err("ユーザー名に使用できない文字が含まれています。")
    }
}

pub struct QuickConnectModal {
    store: Entity<TerminalSessionStore>,
    workspace: WeakEntity<Workspace>,
    name: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    protocol: ConnectionProtocol,
    macro_name: Option<String>,
    log: bool,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl QuickConnectModal {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let single_line = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };

        let name = single_line("接続名 (保存するとき)", window, cx);
        let host = single_line("ホスト名 / IP アドレス", window, cx);
        let port = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("22", window, cx);
            editor
        });
        let user = single_line("ユーザー名 (SSH)", window, cx);

        let log = TerminalSettings::get_global(cx).session_log.auto;
        let focus_handle = cx.focus_handle();
        window.focus(&host.focus_handle(cx), cx);

        Self {
            store,
            workspace,
            name,
            host,
            port,
            user,
            protocol: ConnectionProtocol::Ssh,
            macro_name: None,
            log,
            error: None,
            focus_handle,
        }
    }

    fn set_protocol(
        &mut self,
        protocol: ConnectionProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.protocol == protocol {
            return;
        }
        let previous_default = self.protocol.default_port().to_string();
        self.protocol = protocol;
        // Only rewrite the port when it still holds the other protocol's
        // default, so a hand-typed port survives a protocol switch.
        if self.port.read(cx).text(cx).trim() == previous_default {
            self.port.update(cx, |editor, cx| {
                editor.set_text(protocol.default_port().to_string(), window, cx);
            });
        }
        cx.notify();
    }

    fn form_connection(&self, cx: &App) -> Result<SavedConnection, SharedString> {
        let host = self.host.read(cx).text(cx).trim().to_string();
        validate_host(&host).map_err(SharedString::from)?;
        let user = self.user.read(cx).text(cx).trim().to_string();
        validate_user(&user).map_err(SharedString::from)?;
        let port_text = self.port.read(cx).text(cx).trim().to_string();
        let port = if port_text.is_empty() {
            None
        } else {
            Some(
                port_text
                    .parse::<u16>()
                    .map_err(|_| SharedString::from("ポート番号が不正です。"))?,
            )
        };
        let name = self.name.read(cx).text(cx).trim().to_string();

        Ok(SavedConnection {
            name: if name.is_empty() { host.clone() } else { name },
            protocol: self.protocol,
            host,
            port,
            user: (!user.is_empty()).then_some(user),
            macro_name: self.macro_name.clone(),
            log: Some(self.log),
        })
    }

    fn apply_connection(
        &mut self,
        connection: &SavedConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.protocol = connection.protocol;
        self.macro_name = connection.macro_name.clone();
        self.log = connection
            .log
            .unwrap_or_else(|| TerminalSettings::get_global(cx).session_log.auto);
        self.name.update(cx, |editor, cx| {
            editor.set_text(connection.name.clone(), window, cx)
        });
        self.host.update(cx, |editor, cx| {
            editor.set_text(connection.host.clone(), window, cx)
        });
        self.port.update(cx, |editor, cx| {
            editor.set_text(connection.port_or_default().to_string(), window, cx)
        });
        self.user.update(cx, |editor, cx| {
            editor.set_text(connection.user.clone().unwrap_or_default(), window, cx)
        });
        self.error = None;
        cx.notify();
    }

    fn save_connection(&mut self, cx: &mut Context<Self>) {
        let connection = match self.form_connection(cx) {
            Ok(connection) => connection,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            match file
                .connections
                .iter()
                .position(|candidate| candidate.name == connection.name)
            {
                Some(index) => file.connections[index] = connection,
                None => file.connections.push(connection),
            }
            store.write(file, cx)
        });
        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("保存できませんでした: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn delete_connection(&mut self, name: String, cx: &mut Context<Self>) {
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            file.connections.retain(|candidate| candidate.name != name);
            store.write(file, cx)
        });
        cx.spawn(async move |_, _| {
            write.await.log_err();
        })
        .detach();
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = match self.form_connection(cx) {
            Ok(connection) => connection,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        let terminal_macro = connection
            .macro_name
            .as_deref()
            .and_then(|name| self.store.read(cx).macro_named(name).cloned());
        let start_log = connection.log.unwrap_or(false);
        let workspace = self.workspace.clone();
        cx.emit(DismissEvent);
        connect_and_run(workspace, connection, terminal_macro, start_log, window, cx);
    }
}

/// Spawns the connection's command in a new terminal tab, then optionally
/// starts logging and runs a macro against it.
///
/// The work is owned by the workspace rather than the modal, because the modal
/// is dismissed the moment the user hits 「接続」.
pub fn connect_and_run(
    workspace: WeakEntity<Workspace>,
    connection: SavedConnection,
    terminal_macro: Option<TerminalMacro>,
    start_log: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    workspace_entity.update(cx, |_, cx| {
        cx.spawn_in(window, async move |workspace, cx| {
            let (program, args) = connection.command();
            let lookup_program = program.clone();
            let found = cx
                .background_spawn(async move { which::which(&lookup_program) })
                .await;
            if found.is_err() {
                let message = missing_program_message(&program);
                workspace
                    .update(cx, |workspace, cx| workspace.show_error(message, cx))
                    .ok();
                return;
            }

            let label = format!("{} ({})", connection.host, connection.protocol.label());
            let spawn_task = SpawnInTerminal {
                id: TaskId(format!("terminal::quick-connect::{}", connection.name)),
                full_label: label.clone(),
                label: connection.host.clone(),
                command: Some(program),
                args,
                command_label: label,
                use_new_terminal: true,
                allow_concurrent_runs: true,
                reveal: RevealStrategy::Always,
                reveal_target: RevealTarget::Dock,
                show_summary: false,
                show_command: false,
                show_rerun: true,
                ..SpawnInTerminal::default()
            };

            let Some(spawn) = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.panel::<TerminalPanel>(cx).map(|terminal_panel| {
                        terminal_panel.update(cx, |terminal_panel, cx| {
                            terminal_panel.spawn_task(&spawn_task, window, cx)
                        })
                    })
                })
                .ok()
                .flatten()
            else {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error("ターミナルパネルが利用できません。".to_string(), cx)
                    })
                    .ok();
                return;
            };

            let terminal = match spawn.await {
                Ok(terminal) => terminal,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_error(format!("接続できませんでした: {error}"), cx)
                        })
                        .ok();
                    return;
                }
            };

            let Some(terminal) = terminal.upgrade() else {
                return;
            };
            finish_connection(
                workspace,
                terminal,
                connection,
                terminal_macro,
                start_log,
                cx,
            );
        })
        .detach();
    });
}

fn finish_connection(
    workspace: WeakEntity<Workspace>,
    terminal: Entity<Terminal>,
    connection: SavedConnection,
    terminal_macro: Option<TerminalMacro>,
    start_log: bool,
    cx: &mut AsyncWindowContext,
) {
    workspace
        .update_in(cx, |workspace, _window, cx| {
            // Normally the spawn has already put the terminal in a view, which
            // is what shows macro progress. If it has not, the log and the
            // macro still run — just without the progress strip.
            let view = workspace
                .panel::<TerminalPanel>(cx)
                .and_then(|terminal_panel| {
                    terminal_panel.read(cx).terminal_view_for(&terminal, cx)
                });

            if start_log {
                let host = connection.host.clone();
                let started = match &view {
                    Some(view) => view.update(cx, |view, cx| {
                        view.start_session_log_at_default_path(Some(&host), cx)
                    }),
                    None => terminal.update(cx, |terminal, cx| {
                        let settings = TerminalSettings::get_global(cx).session_log.clone();
                        let path = terminal::session_log::default_log_path(
                            &settings,
                            &terminal.title(false),
                            Some(&host),
                        );
                        terminal.start_session_log(path, settings.options(), cx)
                    }),
                };
                if let Err(error) = started {
                    workspace.show_error(format!("ログを開始できませんでした: {error}"), cx);
                }
            }

            if let Some(terminal_macro) = terminal_macro {
                match &view {
                    Some(view) => view.update(cx, |view, cx| view.run_macro(terminal_macro, cx)),
                    None => terminal::terminal_macro::run_macro(
                        terminal.clone(),
                        terminal_macro,
                        |_, _| {},
                        cx,
                    )
                    .detach_and_log_err(cx),
                }
            }
        })
        .ok();
}

fn missing_program_message(program: &str) -> String {
    if program == "telnet" && cfg!(target_os = "windows") {
        "telnet が見つかりません。Windows の「オプション機能」から「Telnet クライアント」を \
         追加するか、管理者権限の PowerShell で \
         `Enable-WindowsOptionalFeature -Online -FeatureName TelnetClient` を実行してください。"
            .to_string()
    } else {
        format!("{program} が PATH 上に見つかりません。")
    }
}

impl QuickConnectModal {
    fn render_saved_connections(&self, cx: &mut Context<Self>) -> AnyElement {
        let connections = self.store.read(cx).connections().to_vec();
        v_flex()
            .gap_0p5()
            .children(
                connections
                    .into_iter()
                    .enumerate()
                    .map(|(index, connection)| {
                        let name = connection.name.clone();
                        let detail = format!(
                            "{} {}:{}",
                            connection.protocol.label(),
                            connection.host,
                            connection.port_or_default()
                        );
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_between()
                            .child(
                                Button::new(("saved-connection", index), name.clone())
                                    .full_width()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.apply_connection(&connection, window, cx);
                                    })),
                            )
                            .child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(("delete-connection", index), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("この接続先を削除します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.delete_connection(name.clone(), cx);
                                    })),
                            )
                    }),
            )
            .into_any_element()
    }
}

impl Render for QuickConnectModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let macro_names = self.store.read(cx).macros().to_vec();
        let selected_macro_label = self
            .macro_name
            .clone()
            .unwrap_or_else(|| "(なし)".to_string());
        let view = cx.weak_entity();
        let macro_menu = ContextMenu::build(window, cx, {
            move |mut menu, _, _| {
                menu = menu.entry("(なし)", None, {
                    let view = view.clone();
                    move |_, cx| {
                        view.update(cx, |this, cx| {
                            this.macro_name = None;
                            cx.notify();
                        })
                        .ok();
                    }
                });
                for terminal_macro in &macro_names {
                    let name = terminal_macro.name.clone();
                    let view = view.clone();
                    menu = menu.entry(name.clone(), None, move |_, cx| {
                        let name = name.clone();
                        view.update(cx, |this, cx| {
                            this.macro_name = Some(name);
                            cx.notify();
                        })
                        .ok();
                    });
                }
                menu
            }
        });

        let protocol = self.protocol;
        let saved = self.render_saved_connections(cx);

        v_flex()
            .w(rems(38.))
            .elevation_3(cx)
            .key_context("QuickConnect")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.connect(window, cx);
            }))
            .child(
                Modal::new("terminal-quick-connect", None)
                    .header(
                        ModalHeader::new()
                            .headline("クイック接続")
                            .description("SSH または Telnet でネットワーク機器に接続します。"),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("protocol-ssh", "SSH")
                                                .toggle_state(protocol == ConnectionProtocol::Ssh)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.set_protocol(
                                                        ConnectionProtocol::Ssh,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        )
                                        .child(
                                            Button::new("protocol-telnet", "Telnet")
                                                .toggle_state(
                                                    protocol == ConnectionProtocol::Telnet,
                                                )
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.set_protocol(
                                                        ConnectionProtocol::Telnet,
                                                        window,
                                                        cx,
                                                    );
                                                })),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().child(self.host.clone()))
                                        .child(
                                            Label::new("ポート")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(div().w(rems(5.)).child(self.port.clone())),
                                )
                                .when(protocol == ConnectionProtocol::Ssh, |this| {
                                    this.child(self.user.clone())
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new("接続後に実行するマクロ")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            DropdownMenu::new(
                                                "quick-connect-macro",
                                                selected_macro_label,
                                                macro_menu,
                                            )
                                            .style(DropdownStyle::Outlined),
                                        ),
                                )
                                .child(
                                    Checkbox::new("quick-connect-log", self.log.into())
                                        .label("ログを記録する")
                                        .on_click(cx.listener(
                                            |this, state: &ToggleState, _, cx| {
                                                let checked = *state == ToggleState::Selected;
                                                if this.log != checked {
                                                    this.log = checked;
                                                    cx.notify();
                                                }
                                            },
                                        )),
                                )
                                .child(self.name.clone())
                                .when_some(self.error.clone(), |this, error| {
                                    this.child(Label::new(error).color(Color::Error))
                                }),
                        ),
                    )
                    .section(
                        Section::new()
                            .header("保存済みの接続先".into())
                            .child(saved),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot(Button::new("save-connection", "保存").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.save_connection(cx);
                                }),
                            ))
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("cancel", "キャンセル").on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                        ),
                                    )
                                    .child(Button::new("connect", "接続").on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.connect(window, cx);
                                        },
                                    ))),
                            ),
                    ),
            )
    }
}

impl EventEmitter<DismissEvent> for QuickConnectModal {}

impl Focusable for QuickConnectModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for QuickConnectModal {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_round_trip_through_the_editor_fields() {
        assert_eq!(format_seconds(5_000), "5");
        assert_eq!(format_seconds(1_500), "1.5");
        assert_eq!(parse_seconds("5"), Some(5_000));
        assert_eq!(parse_seconds(" 1.5 "), Some(1_500));
        assert_eq!(parse_seconds(""), None);
        assert_eq!(parse_seconds("-1"), None);
        assert_eq!(parse_seconds("abc"), None);
    }

    #[test]
    fn rejects_hosts_the_shell_would_split() {
        assert!(validate_host("192.168.1.1").is_ok());
        assert!(validate_host("rtx1200.corp.example").is_ok());
        assert!(validate_host("[fe80::1]").is_ok());
        assert!(validate_host("").is_err());
        assert!(validate_host("host; rm -rf /").is_err());
        assert!(validate_user("admin").is_ok());
        assert!(validate_user("ad min").is_err());
    }
}
