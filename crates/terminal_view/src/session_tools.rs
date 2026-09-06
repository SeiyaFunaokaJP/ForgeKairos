//! TeraTerm-style session tooling: quick connect, the macro picker and the
//! macro editor.
//!
//! The data these modals read and write lives in `terminal_sessions.json`
//! (see [`terminal::terminal_macro`]); this module is only the UI over it.

use std::sync::{Arc, atomic::AtomicBool};

use collections::HashSet;
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render,
    Subscription, Task, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::Fs;
use settings::Settings as _;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal::{
    Terminal,
    quick_connect_credentials::{self, QuickConnectCredentials, StoredPassword},
    terminal_macro::{
        ConnectionProtocol, MacroStep, SavedConnection, TerminalMacro, TerminalSessionStore,
        normalize_folder, normalize_tags,
    },
    terminal_settings::TerminalSettings,
};
use ui::{
    Checkbox, ContextMenu, Disclosure, DropdownMenu, DropdownStyle, HighlightedLabel,
    InputFrameSize, ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section,
    ToggleState, Tooltip, input_frame, input_frame_sized, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Panel as _, Workspace};
use zed_actions::terminal::{InstallClaudeCode, LaunchClaudeCode};

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
        workspace.register_action(|workspace, _: &InstallClaudeCode, window, cx| {
            spawn_claude_code_command(workspace, ClaudeCodeCommand::Install, window, cx);
        });
        workspace.register_action(|workspace, action: &LaunchClaudeCode, window, cx| {
            spawn_claude_code_command(
                workspace,
                ClaudeCodeCommand::Launch {
                    effort: action.effort.clone(),
                },
                window,
                cx,
            );
        });
    })
    .detach();
}

/// 公式パッケージ名。フォークは Claude Code を同梱せず (ADR-F20)、公式の npm
/// インストールコマンドをそのまま実行するだけにする。Zed 同梱の Node ランタイムは
/// 使わず、ユーザーの PATH 上の `npm` を使う。
const CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code";
const NPM_PROGRAM: &str = "npm";
const CLAUDE_CODE_PROGRAM: &str = "claude";

enum ClaudeCodeCommand {
    Install,
    Launch { effort: Option<String> },
}

/// 公式 CLI を統合ターミナルの新しいタブで実行する。
///
/// ここで組み立てるコマンドは**人間がターミナルに打つのと同じ実行経路**なので、
/// ADR-F18 (リモートのエージェント実行と違い人間のターミナルは対象外) /
/// ADR-F19 と同じ扱いで **egress 許可リストの対象外**。プロキシ注入も
/// 許可リストへのホスト追加も不要で、ここでは一切行わない。
fn spawn_claude_code_command(
    workspace: &mut Workspace,
    command: ClaudeCodeCommand,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let (program, args, task_id, label) = match &command {
        ClaudeCodeCommand::Install => (
            NPM_PROGRAM.to_string(),
            vec![
                "install".to_string(),
                "-g".to_string(),
                CLAUDE_CODE_PACKAGE.to_string(),
            ],
            "terminal::install-claude-code",
            "Claude Code をインストール".to_string(),
        ),
        ClaudeCodeCommand::Launch { effort } => {
            let mut args = Vec::new();
            if let Some(effort) = effort {
                // `--effort <level>` は公式 CLI のオプション。レベル名は CLI が
                // 受け取る値をそのまま渡す (訳さない)。
                args.push("--effort".to_string());
                args.push(effort.clone());
            }
            (
                CLAUDE_CODE_PROGRAM.to_string(),
                args,
                "terminal::launch-claude-code",
                "Claude Code".to_string(),
            )
        }
    };

    let command_label = if args.is_empty() {
        program.clone()
    } else {
        format!("{program} {}", args.join(" "))
    };
    let cwd = crate::first_project_directory(workspace, cx);
    // リモートプロジェクトではコマンドはリモートのシェルで動くので、
    // ローカルの PATH を調べても意味がない (事前チェックを飛ばす)。
    let check_local_path = !workspace.project().read(cx).is_remote();

    cx.spawn_in(window, async move |workspace, cx| {
        if check_local_path {
            let lookup_program = program.clone();
            let found = cx
                .background_spawn(async move { which::which(&lookup_program) })
                .await;
            if found.is_err() {
                let message = claude_code_missing_program_message(&program);
                workspace
                    .update(cx, |workspace, cx| workspace.show_error(message, cx))
                    .ok();
                return;
            }
        }

        let spawn_task = SpawnInTerminal {
            id: TaskId(task_id.to_string()),
            full_label: label.clone(),
            label,
            command: Some(program),
            args,
            command_label,
            cwd,
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            show_summary: false,
            // 何が実行されるのかを利用者がその場で確認できるよう、
            // 実行するコマンド行をターミナルに表示する。
            show_command: true,
            show_rerun: true,
            ..SpawnInTerminal::default()
        };

        let Some(spawn) = spawn_task_in_terminal_panel(&workspace, &spawn_task, cx) else {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error("ターミナルパネルが利用できません。".to_string(), cx)
                })
                .ok();
            return;
        };

        if let Err(error) = spawn.await {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(
                        format!("ターミナルでコマンドを実行できませんでした: {error}"),
                        cx,
                    )
                })
                .ok();
        }
    })
    .detach();
}

/// Runs `spawn_task` in the workspace's terminal panel.
///
/// The panel is looked up in its own short read and only updated once the
/// workspace lease has been released: [`TerminalPanel::spawn_task`] reads the
/// workspace itself, so calling it from inside `workspace.update_in` leases the
/// workspace twice and panics. On Windows that panic surfaces as the window
/// freezing before it goes down, which is what a click on the Claude Code
/// buttons (and on 「接続」 in quick connect) used to do.
fn spawn_task_in_terminal_panel(
    workspace: &WeakEntity<Workspace>,
    spawn_task: &SpawnInTerminal,
    cx: &mut AsyncWindowContext,
) -> Option<Task<anyhow::Result<WeakEntity<Terminal>>>> {
    let terminal_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))
        .ok()
        .flatten()?;
    terminal_panel
        .update_in(cx, |terminal_panel, window, cx| {
            terminal_panel.spawn_task(spawn_task, window, cx)
        })
        .ok()
}

fn claude_code_missing_program_message(program: &str) -> String {
    if program == NPM_PROGRAM {
        "npm が PATH 上に見つかりません。Claude Code のインストールには Node.js が必要です。\
         https://nodejs.org/ から Node.js をインストールしてから、もう一度お試しください。"
            .to_string()
    } else {
        "claude が PATH 上に見つかりません。先に「Claude Code をインストール (公式コマンド)」\
         を実行してください。"
            .to_string()
    }
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
    _subscriptions: [Subscription; 2],
}

impl MacroPicker {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = MacroPickerDelegate {
            store: store.clone(),
            workspace,
            macros: Vec::new(),
            entries: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        // The store loads `terminal_sessions.json` asynchronously, so the first
        // time this modal is opened it is built against an empty store and has
        // to re-query once the file lands.
        let store_loaded = cx.observe_in(&store, window, |this, _, window, cx| {
            this.picker
                .update(cx, |picker, cx| picker.refresh(window, cx));
        });
        // The modal layer only listens to this wrapper, so the picker's own
        // dismissals (Esc, confirm) have to be forwarded or the modal never
        // closes.
        let picker_dismissed = cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| {
            cx.emit(DismissEvent);
        });
        Self {
            picker,
            _subscriptions: [store_loaded, picker_dismissed],
        }
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

/// One row of the macro list: either a folder heading or a macro.
enum MacroPickerEntry {
    Folder(SharedString),
    Macro {
        index: usize,
        /// Byte offsets of the matched characters within the macro's *name*,
        /// which is the only part of the search text the row shows.
        name_highlights: Vec<usize>,
    },
}

/// What the picker searches: `フォルダー/名前`, then the description, then the
/// tags, so typing a folder name narrows to that folder and typing a tag finds
/// every macro carrying it.
fn macro_search_text(terminal_macro: &TerminalMacro) -> String {
    let mut text = terminal_macro.qualified_name();
    if !terminal_macro.description.is_empty() {
        text.push(' ');
        text.push_str(&terminal_macro.description);
    }
    for tag in &terminal_macro.tags {
        text.push(' ');
        text.push_str(tag);
    }
    text
}

/// Macros are always listed folder first, then by name, whether or not a query
/// narrowed them, so the same macro is always found in the same place.
fn sort_macros(macros: &mut [TerminalMacro]) {
    macros.sort_by(|left, right| {
        left.folder_display()
            .cmp(right.folder_display())
            .then_with(|| left.name.cmp(&right.name))
    });
}

pub struct MacroPickerDelegate {
    store: Entity<TerminalSessionStore>,
    workspace: WeakEntity<Workspace>,
    /// Sorted by folder then name; [`MacroPickerDelegate::entries`] indexes it.
    macros: Vec<TerminalMacro>,
    entries: Vec<MacroPickerEntry>,
    selected_index: usize,
}

impl MacroPickerDelegate {
    fn selected_macro(&self) -> Option<&TerminalMacro> {
        match self.entries.get(self.selected_index)? {
            MacroPickerEntry::Macro { index, .. } => self.macros.get(*index),
            MacroPickerEntry::Folder(_) => None,
        }
    }

    fn first_selectable_index(&self) -> usize {
        self.entries
            .iter()
            .position(|entry| matches!(entry, MacroPickerEntry::Macro { .. }))
            .unwrap_or(0)
    }

    /// Turns the matches into rows, inserting a heading whenever the folder
    /// changes. Macros with no folder sort first and get no heading.
    fn build_entries(
        macros: &[TerminalMacro],
        mut matches: Vec<StringMatch>,
    ) -> Vec<MacroPickerEntry> {
        // `match_strings` returns best-scoring first; the list is ordered by
        // folder and name instead, and `candidate_id` is that position.
        matches.sort_by_key(|string_match| string_match.candidate_id);

        let mut entries = Vec::with_capacity(matches.len());
        let mut current_folder: Option<&str> = None;
        for string_match in matches {
            let Some(terminal_macro) = macros.get(string_match.candidate_id) else {
                continue;
            };
            let folder = terminal_macro.folder_display();
            if current_folder != Some(folder) {
                current_folder = Some(folder);
                if !folder.is_empty() {
                    entries.push(MacroPickerEntry::Folder(folder.to_string().into()));
                }
            }
            entries.push(MacroPickerEntry::Macro {
                index: string_match.candidate_id,
                name_highlights: name_highlights(terminal_macro, &string_match.positions),
            });
        }
        entries
    }

    fn open_editor(
        &self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let store = self.store.clone();
        let workspace = self.workspace.clone();
        // Swapping modals goes through `ModalLayer::hide_modal`, which reads the
        // outgoing modal's focus handle — and `MacroPicker` forwards that to this
        // very picker. Doing it inline would read the picker while this handler
        // still has it leased, which panics with a double lease (on Windows the
        // window stops repainting first, so it reads as a freeze). Deferring lets
        // the effect cycle return the picker first; `toggle_modal` then closes
        // this picker and opens the editor as one swap, so no separate
        // `DismissEvent` is emitted that could race ahead and close the editor.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        MacroEditor::new(store, edited, window, cx)
                    });
                })
                .log_err();
        });
    }

    /// Swaps in the 「回数を指定して実行」 form. Deferred for the same reason as
    /// [`MacroPickerDelegate::open_editor`].
    fn open_run_options(
        &self,
        terminal_macro: TerminalMacro,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            let handle = workspace.clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        MacroRunOptions::new(terminal_macro, handle, window, cx)
                    });
                })
                .log_err();
        });
    }
}

// -------------------------------------------------------------------------
// Run options (繰り返し回数の指定)
// -------------------------------------------------------------------------

/// Runs one saved macro with its repetition overridden, without touching what
/// is stored on disk.
pub struct MacroRunOptions {
    terminal_macro: TerminalMacro,
    workspace: WeakEntity<Workspace>,
    count: Entity<Editor>,
    interval: Entity<Editor>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
}

impl MacroRunOptions {
    pub fn new(
        terminal_macro: TerminalMacro,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let count = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(terminal_macro.repeat.to_string(), window, cx);
            editor
        });
        let interval = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(
                format_seconds(terminal_macro.repeat_interval_ms),
                window,
                cx,
            );
            editor
        });
        let focus_handle = cx.focus_handle();
        window.focus(&count.focus_handle(cx), cx);
        Self {
            terminal_macro,
            workspace,
            count,
            interval,
            error: None,
            focus_handle,
        }
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(repeat) = self.count.read(cx).text(cx).trim().parse::<u32>() else {
            self.error = Some("繰り返し回数は 0 以上の整数で入力してください。".into());
            cx.notify();
            return;
        };
        let Some(repeat_interval_ms) = parse_seconds(&self.interval.read(cx).text(cx)) else {
            self.error = Some("間隔は 0 以上の秒数で入力してください。".into());
            cx.notify();
            return;
        };

        let terminal_macro = TerminalMacro {
            repeat,
            repeat_interval_ms,
            ..self.terminal_macro.clone()
        };
        let outcome = self.workspace.update(cx, |workspace, cx| {
            run_macro_on_active_terminal(workspace, terminal_macro, window, cx)
        });
        if let Ok(Err(message)) = outcome {
            self.error = Some(message.into());
            cx.notify();
            return;
        }
        cx.emit(DismissEvent);
    }
}

impl Render for MacroRunOptions {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.terminal_macro.name.clone();
        let count_field = input_frame(self.count.focus_handle(cx).contains_focused(window, cx), cx)
            .w(rems(6.))
            .child(self.count.clone());
        let interval_field = input_frame(
            self.interval.focus_handle(cx).contains_focused(window, cx),
            cx,
        )
        .w(rems(6.))
        .child(self.interval.clone());
        v_flex()
            .w(rems(28.))
            .elevation_3(cx)
            .key_context("TerminalMacroRunOptions")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.run(window, cx);
            }))
            .child(
                Modal::new("terminal-macro-run-options", None)
                    .header(
                        ModalHeader::new()
                            .headline("回数を指定して実行")
                            .description(format!("マクロ「{name}」を繰り返し実行します。")),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new("繰り返し回数")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(count_field)
                                        .child(
                                            Label::new("(0 は中止まで繰り返します)")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new("間隔 (秒)")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(interval_field),
                                )
                                .when_some(self.error.clone(), |this, error| {
                                    this.child(Label::new(error).color(Color::Error))
                                }),
                        ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new("cancel", "キャンセル")
                                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                                )
                                .child(Button::new("run", "実行").on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.run(window, cx);
                                    },
                                ))),
                        ),
                    ),
            )
    }
}

impl EventEmitter<DismissEvent> for MacroRunOptions {}

impl Focusable for MacroRunOptions {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for MacroRunOptions {}

impl PickerDelegate for MacroPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "TerminalMacroPicker"
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
        cx.notify();
    }

    fn can_select(&self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) -> bool {
        matches!(self.entries.get(ix), Some(MacroPickerEntry::Macro { .. }))
    }

    fn separators_after_indices(&self) -> Vec<usize> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                matches!(entry, MacroPickerEntry::Folder(_))
                    .then(|| index.checked_sub(1))
                    .flatten()
            })
            .collect()
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "フォルダー / 名前 / タグで絞り込み…".into()
    }

    fn no_matches_text(&self, _: &mut Window, cx: &mut App) -> Option<SharedString> {
        if self.store.read(cx).is_loaded() {
            Some("保存済みのマクロがありません".into())
        } else {
            Some("読み込み中…".into())
        }
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros);
        let candidates = macros
            .iter()
            .enumerate()
            .map(|(index, terminal_macro)| {
                StringMatchCandidate::new(index, &macro_search_text(terminal_macro))
            })
            .collect::<Vec<_>>();
        let query = query.trim().to_string();
        let executor = cx.background_executor().clone();

        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                candidates.len(),
                &AtomicBool::new(false),
                executor,
            )
            .await;
            picker
                .update(cx, |picker, cx| {
                    let delegate = &mut picker.delegate;
                    delegate.entries = Self::build_entries(&macros, matches);
                    delegate.macros = macros;
                    delegate.selected_index = delegate.first_selectable_index();
                    cx.notify();
                })
                .log_err();
        })
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
        match self.entries.get(ix)? {
            MacroPickerEntry::Folder(folder) => Some(
                h_flex()
                    .w_full()
                    .px_2()
                    .pt_1()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Folder)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(folder.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element(),
            ),
            MacroPickerEntry::Macro {
                index,
                name_highlights,
            } => {
                let terminal_macro = self.macros.get(*index)?;
                let mut detail = if terminal_macro.description.is_empty() {
                    format!("{} ステップ", terminal_macro.steps.len())
                } else {
                    format!(
                        "{} ({} ステップ)",
                        terminal_macro.description,
                        terminal_macro.steps.len()
                    )
                };
                if let Some(count) = terminal_macro.iteration_count() {
                    if count > 1 {
                        detail.push_str(&format!(" / {count} 回繰り返し"));
                    }
                } else {
                    detail.push_str(" / 中止まで繰り返し");
                }
                if !terminal_macro.tags.is_empty() {
                    detail.push_str(&format!(" / {}", terminal_macro.tags.join(", ")));
                }
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(
                            v_flex()
                                .child(HighlightedLabel::new(
                                    terminal_macro.name.clone(),
                                    name_highlights.clone(),
                                ))
                                .child(
                                    Label::new(detail)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .into_any_element(),
                )
            }
        }
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
                .child(
                    Button::new("run-macro-repeatedly", "回数を指定して実行…")
                        .tooltip(Tooltip::text(
                            "繰り返し回数と間隔を指定して、このマクロを実行します。",
                        ))
                        .on_click(cx.listener(|picker, _, window, cx| {
                            let selected = picker.delegate.selected_macro().cloned();
                            if let Some(terminal_macro) = selected {
                                picker.delegate.open_run_options(terminal_macro, window, cx);
                            }
                        })),
                )
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

/// Maps the match positions of the whole search text onto the macro's name,
/// which is the only part of it the row renders.
fn name_highlights(terminal_macro: &TerminalMacro, positions: &[usize]) -> Vec<usize> {
    let start = match &terminal_macro.folder {
        // `qualified_name` is `folder` + `/` + `name`, and the positions are
        // byte offsets into it.
        Some(folder) => folder.len() + 1,
        None => 0,
    };
    let end = start + terminal_macro.name.len();
    positions
        .iter()
        .filter(|position| (start..end).contains(*position))
        .map(|position| position - start)
        .collect()
}

// -------------------------------------------------------------------------
// Macro editor
// -------------------------------------------------------------------------

/// Shown when a save is attempted before `terminal_sessions.json` has been read
/// once; writing then would rewrite the file from an empty in-memory copy.
const LOADING_ERROR: &str =
    "設定ファイルの読み込みが終わっていません。少し待ってからもう一度お試しください。";

#[derive(Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Send,
    WaitFor,
    Sleep,
    Clear,
    Repeat,
}

impl StepKind {
    fn label(&self) -> &'static str {
        match self {
            StepKind::Send => "送信",
            StepKind::WaitFor => "期待文字を待つ",
            StepKind::Sleep => "秒待ち",
            StepKind::Clear => "画面を消去",
            StepKind::Repeat => "繰り返し",
        }
    }

    /// Whether the row shows the text field (期待文字 / 送信する文字列).
    fn has_text(&self) -> bool {
        matches!(self, StepKind::Send | StepKind::WaitFor)
    }

    /// Whether the row shows the seconds field.
    fn has_duration(&self) -> bool {
        matches!(self, StepKind::WaitFor | StepKind::Sleep)
    }

    const ALL: [StepKind; 5] = [
        StepKind::Send,
        StepKind::WaitFor,
        StepKind::Sleep,
        StepKind::Clear,
        StepKind::Repeat,
    ];

    /// Kinds offered inside a 繰り返し. A second level of nesting is only
    /// reachable by editing `terminal_sessions.json` by hand, so the form does
    /// not offer to create one.
    const NESTABLE: [StepKind; 4] = [
        StepKind::Send,
        StepKind::WaitFor,
        StepKind::Sleep,
        StepKind::Clear,
    ];
}

/// Where a row sits in the form. The form draws one level of nesting: the
/// steps of a top-level 繰り返し.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StepPath {
    Top(usize),
    Child { parent: usize, child: usize },
}

impl StepPath {
    fn index(&self) -> usize {
        match self {
            StepPath::Top(index) => *index,
            StepPath::Child { child, .. } => *child,
        }
    }

    /// 「3」 for a top-level step, 「3-2」 for the second step of the 繰り返し
    /// that is the third step of the macro. Matches how a failing run names
    /// the step it stopped on.
    fn label(&self) -> String {
        match self {
            StepPath::Top(index) => (index + 1).to_string(),
            StepPath::Child { parent, child } => format!("{}-{}", parent + 1, child + 1),
        }
    }
}

/// One editable row. The editors travel with the row when it is reordered, so
/// a moved row never picks up the value of the row that took its place.
struct StepRow {
    kind: StepKind,
    text: Entity<Editor>,
    number: Entity<Editor>,
    /// Repetitions of a 繰り返し row. Separate from `number`, which is a
    /// duration in seconds.
    count: Entity<Editor>,
    regex: bool,
    newline: bool,
    /// Steps of a 繰り返し row.
    children: Vec<StepRow>,
    /// The step this row stands for when the form cannot draw it — a 繰り返し
    /// nested two deep. Saving writes it back exactly as it was read.
    preserved: Option<MacroStep>,
}

impl StepRow {
    /// `nested` marks a row that already lives inside a 繰り返し, where a
    /// further 繰り返し is shown read-only instead of being unfolded.
    fn new(step: &MacroStep, nested: bool, window: &mut Window, cx: &mut App) -> Self {
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
            MacroStep::Clear => (StepKind::Clear, String::new(), String::new(), false, true),
            MacroStep::Repeat { .. } => {
                (StepKind::Repeat, String::new(), String::new(), false, true)
            }
        };

        let (count_value, children, preserved) = match step {
            MacroStep::Repeat { count, steps } if !nested => (
                count.to_string(),
                steps
                    .iter()
                    .map(|step| StepRow::new(step, true, window, cx))
                    .collect(),
                None,
            ),
            MacroStep::Repeat { count, .. } => (count.to_string(), Vec::new(), Some(step.clone())),
            _ => ("1".to_string(), Vec::new(), None),
        };

        let single_line = |value: String, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(value, window, cx);
                editor
            })
        };
        let text = single_line(text_value, window, cx);
        let number = single_line(number_value, window, cx);
        let count = single_line(count_value, window, cx);

        Self {
            kind,
            text,
            number,
            count,
            regex,
            newline,
            children,
            preserved,
        }
    }

    /// The row's identity for element ids: stable while the row lives, and
    /// carried along when the row is moved.
    fn element_id(&self) -> gpui::EntityId {
        self.text.entity_id()
    }

    fn to_step(&self, label: &str, cx: &App) -> Result<MacroStep, String> {
        if let Some(preserved) = &self.preserved {
            return Ok(preserved.clone());
        }
        let text = self.text.read(cx).text(cx);
        match self.kind {
            StepKind::Send => Ok(MacroStep::Send {
                text,
                newline: self.newline,
            }),
            StepKind::WaitFor => {
                if text.trim().is_empty() {
                    return Err(format!("ステップ {label}: 期待文字を入力してください。"));
                }
                if self.regex && regex::Regex::new(&text).is_err() {
                    return Err(format!("ステップ {label}: 正規表現が不正です。"));
                }
                Ok(MacroStep::WaitFor {
                    pattern: text,
                    regex: self.regex,
                    timeout_ms: parse_seconds(&self.number.read(cx).text(cx))
                        .ok_or_else(|| format!("ステップ {label}: 秒数が不正です。"))?,
                })
            }
            StepKind::Sleep => Ok(MacroStep::Sleep {
                ms: parse_seconds(&self.number.read(cx).text(cx))
                    .ok_or_else(|| format!("ステップ {label}: 秒数が不正です。"))?,
            }),
            StepKind::Clear => Ok(MacroStep::Clear),
            StepKind::Repeat => {
                let count = self
                    .count
                    .read(cx)
                    .text(cx)
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        format!("ステップ {label}: 繰り返し回数は 0 以上の整数で入力してください。")
                    })?;
                let mut steps = Vec::with_capacity(self.children.len());
                for (index, child) in self.children.iter().enumerate() {
                    steps.push(child.to_step(&format!("{label}-{}", index + 1), cx)?);
                }
                Ok(MacroStep::Repeat { count, steps })
            }
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

/// Heading the tree files macros without a folder under. Only drawn once at
/// least one folder exists, so a user who never made a folder is not shown a
/// grouping they did not ask for.
const UNFILED_FOLDER: &str = "未分類";

/// A macro dragged from the tree onto a folder heading.
#[derive(Clone)]
struct DraggedMacro {
    name: String,
}

/// What follows the pointer while a macro is dragged between folders.
struct DraggedMacroLabel(SharedString);

impl Render for DraggedMacroLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .elevation_2(cx)
            .child(Label::new(self.0.clone()).size(LabelSize::Small))
    }
}

/// One row of the saved-macro tree.
enum MacroTreeRow {
    /// A grouping heading; `folder` is `None` for [`UNFILED_FOLDER`].
    Folder {
        folder: Option<String>,
        collapsed: bool,
        count: usize,
    },
    /// A macro filed under the heading above it.
    Macro(TerminalMacro),
}

/// The folder `folder` becomes when `from` is renamed to `to`, or `None` when
/// the rename does not reach it. Sub-folders travel with their parent, so
/// renaming 「拠点」 also renames 「拠点/本社」.
fn folder_after_rename(folder: &str, from: &str, to: &str) -> Option<String> {
    if folder == from {
        return Some(to.to_string());
    }
    folder
        .strip_prefix(from)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| format!("{to}/{rest}"))
}

/// The raw text of every field on the form. Comparing one of these against the
/// snapshot taken when the macro was loaded is how the editor knows the form
/// holds unsaved work: raw text rather than a parsed [`TerminalMacro`], so a
/// half-typed number counts as a change too.
#[derive(PartialEq, Eq)]
struct FormSnapshot {
    name: String,
    folder: String,
    tags: String,
    description: String,
    repeat: String,
    repeat_interval: String,
    steps: Vec<StepSnapshot>,
}

#[derive(PartialEq, Eq)]
struct StepSnapshot {
    kind: StepKind,
    text: String,
    number: String,
    count: String,
    regex: bool,
    newline: bool,
    children: Vec<StepSnapshot>,
    preserved: Option<MacroStep>,
}

impl StepRow {
    fn snapshot(&self, cx: &App) -> StepSnapshot {
        StepSnapshot {
            kind: self.kind,
            text: self.text.read(cx).text(cx),
            number: self.number.read(cx).text(cx),
            count: self.count.read(cx).text(cx),
            regex: self.regex,
            newline: self.newline,
            children: self.children.iter().map(|row| row.snapshot(cx)).collect(),
            preserved: self.preserved.clone(),
        }
    }
}

pub struct MacroEditor {
    store: Entity<TerminalSessionStore>,
    /// Name the macro had when the form was last loaded, so a rename replaces
    /// the original entry instead of adding a second one.
    original_name: Option<String>,
    name: Entity<Editor>,
    folder: Entity<Editor>,
    tags: Entity<Editor>,
    description: Entity<Editor>,
    repeat: Entity<Editor>,
    repeat_interval: Entity<Editor>,
    /// Narrows the saved-macro tree only; it has nothing to do with the form.
    list_filter: Entity<Editor>,
    /// Field values as they stood when the form was loaded. `None` marks a form
    /// that stands for nothing on disk (a 複製), which counts as unsaved from
    /// the moment it is made.
    saved_snapshot: Option<FormSnapshot>,
    /// Collapsed headings of the tree; `None` is the [`UNFILED_FOLDER`] one.
    collapsed_folders: HashSet<Option<String>>,
    /// Folder whose name is being edited in the tree, with the field holding
    /// the new name.
    renaming_folder: Option<(String, Entity<Editor>)>,
    steps: Vec<StepRow>,
    error: Option<SharedString>,
    /// Macro whose 削除 has been asked for but not yet confirmed.
    pending_delete: Option<String>,
    /// Macro picked in the tree while the form held unsaved edits, kept until
    /// the user says whether to throw those edits away. The inner `None` is
    /// 新規.
    pending_selection: Option<Option<TerminalMacro>>,
    focus_handle: FocusHandle,
    _subscriptions: [Subscription; 2],
}

impl MacroEditor {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder_editor = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor
            })
        };

        let name = placeholder_editor("マクロ名", window, cx);
        let folder = placeholder_editor("フォルダー (例: 拠点/本社。省略可)", window, cx);
        let tags = placeholder_editor("タグ (カンマ区切り、省略可)", window, cx);
        let description = placeholder_editor("説明 (省略可)", window, cx);
        let repeat = cx.new(|cx| Editor::single_line(window, cx));
        let repeat_interval = cx.new(|cx| Editor::single_line(window, cx));
        let list_filter = placeholder_editor("フォルダー / 名前 / タグで絞り込み", window, cx);

        let focus_handle = cx.focus_handle();
        // `terminal_sessions.json` is read asynchronously, so an editor opened
        // before the first read landed would otherwise show an empty tree of
        // saved macros for as long as it stays open.
        let store_loaded = cx.observe(&store, |_, _, cx| cx.notify());
        // The tree is rebuilt during render, so the filter field only changes
        // it if the editor itself is re-rendered on every keystroke.
        let filter_edited = cx.subscribe(&list_filter, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        });

        let mut this = Self {
            store,
            original_name: None,
            name,
            folder,
            tags,
            description,
            repeat,
            repeat_interval,
            list_filter,
            saved_snapshot: None,
            collapsed_folders: HashSet::default(),
            renaming_folder: None,
            steps: Vec::new(),
            error: None,
            pending_delete: None,
            pending_selection: None,
            focus_handle,
            _subscriptions: [store_loaded, filter_edited],
        };
        this.load_macro(edited, window, cx);
        this
    }

    /// Points the form at `edited`, or at a blank new macro when it is `None`.
    fn load_macro(
        &mut self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edited = edited.unwrap_or_default();
        self.original_name = (!edited.name.is_empty()).then(|| edited.name.clone());
        self.name.update(cx, |editor, cx| {
            editor.set_text(edited.name, window, cx);
        });
        self.folder.update(cx, |editor, cx| {
            editor.set_text(edited.folder.clone().unwrap_or_default(), window, cx);
        });
        self.tags.update(cx, |editor, cx| {
            editor.set_text(edited.tags.join(", "), window, cx);
        });
        self.description.update(cx, |editor, cx| {
            editor.set_text(edited.description, window, cx);
        });
        self.repeat.update(cx, |editor, cx| {
            editor.set_text(edited.repeat.to_string(), window, cx);
        });
        self.repeat_interval.update(cx, |editor, cx| {
            editor.set_text(format_seconds(edited.repeat_interval_ms), window, cx);
        });
        self.steps = edited
            .steps
            .iter()
            .map(|step| StepRow::new(step, false, window, cx))
            .collect();
        self.error = None;
        self.pending_delete = None;
        self.pending_selection = None;
        self.saved_snapshot = Some(self.snapshot(cx));
        window.focus(&self.name.focus_handle(cx), cx);
        cx.notify();
    }

    fn snapshot(&self, cx: &App) -> FormSnapshot {
        FormSnapshot {
            name: self.name.read(cx).text(cx),
            folder: self.folder.read(cx).text(cx),
            tags: self.tags.read(cx).text(cx),
            description: self.description.read(cx).text(cx),
            repeat: self.repeat.read(cx).text(cx),
            repeat_interval: self.repeat_interval.read(cx).text(cx),
            steps: self.steps.iter().map(|row| row.snapshot(cx)).collect(),
        }
    }

    /// Whether leaving the form now would throw work away.
    fn has_unsaved_edits(&self, cx: &App) -> bool {
        match &self.saved_snapshot {
            Some(snapshot) => *snapshot != self.snapshot(cx),
            None => true,
        }
    }

    /// Loads `edited` into the form, asking first when that would discard
    /// unsaved edits.
    fn select_macro(
        &mut self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clicking the row that is already open is not a request to reload it,
        // and reloading would throw away whatever is being typed into it.
        if let Some(edited) = &edited
            && self.original_name.as_deref() == Some(edited.name.as_str())
        {
            return;
        }
        if self.has_unsaved_edits(cx) {
            self.pending_selection = Some(edited);
            self.error = None;
            cx.notify();
            return;
        }
        self.load_macro(edited, window, cx);
    }

    fn confirm_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edited) = self.pending_selection.take() else {
            return;
        };
        self.load_macro(edited, window, cx);
    }

    /// Turns the form into a new, unsaved macro carrying the same steps, so a
    /// variant of an existing macro does not have to be retyped. Nothing is
    /// written until 保存.
    fn duplicate_macro(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let copy_name = self.unused_copy_name(&self.name.read(cx).text(cx), cx);
        self.original_name = None;
        self.name.update(cx, |editor, cx| {
            editor.set_text(copy_name, window, cx);
        });
        // A copy stands for nothing on disk, so switching away from it has to
        // warn even though no field has been touched since.
        self.saved_snapshot = None;
        self.pending_delete = None;
        self.pending_selection = None;
        self.error = None;
        window.focus(&self.name.focus_handle(cx), cx);
        cx.notify();
    }

    /// `名前 のコピー`, numbered when that is taken, so a copy never silently
    /// overwrites the macro it was made from.
    fn unused_copy_name(&self, base: &str, cx: &App) -> String {
        let base = base.trim();
        let base = if base.is_empty() { "マクロ" } else { base };
        let candidate = format!("{base} のコピー");
        if self.store.read(cx).macro_named(&candidate).is_none() {
            return candidate;
        }
        // There are only as many taken names as there are macros, so one of
        // these suffixes is always free.
        let limit = self.store.read(cx).macros().len() + 2;
        for suffix in 2..=limit {
            let numbered = format!("{base} のコピー {suffix}");
            if self.store.read(cx).macro_named(&numbered).is_none() {
                return numbered;
            }
        }
        candidate
    }

    /// The saved macros the tree shows, in folder-then-name order. Read
    /// straight from the store so the tree is whatever is on disk right now,
    /// not a copy taken at open time.
    fn listed_macros(&self, cx: &App) -> Vec<TerminalMacro> {
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros);
        let filter = self.list_filter.read(cx).text(cx).trim().to_lowercase();
        if !filter.is_empty() {
            macros.retain(|terminal_macro| {
                macro_search_text(terminal_macro)
                    .to_lowercase()
                    .contains(&filter)
            });
        }
        macros
    }

    /// Folder names already in use, offered as completions for the form's
    /// folder field.
    fn known_folders(&self, cx: &App) -> Vec<String> {
        let mut folders = self
            .store
            .read(cx)
            .macros()
            .iter()
            .filter_map(|terminal_macro| terminal_macro.folder.clone())
            .collect::<Vec<_>>();
        folders.sort();
        folders.dedup();
        folders
    }

    /// The tree the left pane draws: a heading per folder, then the macros in
    /// it. Built from the sorted list, so one pass is enough.
    fn macro_tree(&self, cx: &App) -> Vec<MacroTreeRow> {
        // Whether the user has any folders at all, rather than whether the
        // filtered list happens to show one, so the 未分類 heading does not
        // appear and disappear while typing in the filter.
        let unfiled_heading = !self.known_folders(cx).is_empty();

        let mut groups: Vec<(Option<String>, Vec<TerminalMacro>)> = Vec::new();
        for terminal_macro in self.listed_macros(cx) {
            let folder = terminal_macro.folder.clone();
            match groups.last_mut() {
                Some((current, entries)) if *current == folder => entries.push(terminal_macro),
                _ => groups.push((folder, vec![terminal_macro])),
            }
        }

        let mut rows = Vec::new();
        for (folder, entries) in groups {
            let heading = folder.is_some() || unfiled_heading;
            let collapsed = heading && self.collapsed_folders.contains(&folder);
            if heading {
                rows.push(MacroTreeRow::Folder {
                    folder,
                    collapsed,
                    count: entries.len(),
                });
            }
            if !collapsed {
                rows.extend(entries.into_iter().map(MacroTreeRow::Macro));
            }
        }
        rows
    }

    fn blank_step(kind: StepKind) -> MacroStep {
        match kind {
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
            StepKind::Clear => MacroStep::Clear,
            StepKind::Repeat => MacroStep::Repeat {
                count: 1,
                steps: Vec::new(),
            },
        }
    }

    fn add_step(&mut self, kind: StepKind, window: &mut Window, cx: &mut Context<Self>) {
        let step = Self::blank_step(kind);
        self.steps.push(StepRow::new(&step, false, window, cx));
        cx.notify();
    }

    /// Appends a step to the 繰り返し at `parent`.
    fn add_child_step(
        &mut self,
        parent: usize,
        kind: StepKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = Self::blank_step(kind);
        let row = StepRow::new(&step, true, window, cx);
        let Some(parent_row) = self.steps.get_mut(parent) else {
            return;
        };
        parent_row.children.push(row);
        cx.notify();
    }

    fn row(&self, path: StepPath) -> Option<&StepRow> {
        match path {
            StepPath::Top(index) => self.steps.get(index),
            StepPath::Child { parent, child } => self.steps.get(parent)?.children.get(child),
        }
    }

    fn row_mut(&mut self, path: StepPath) -> Option<&mut StepRow> {
        match path {
            StepPath::Top(index) => self.steps.get_mut(index),
            StepPath::Child { parent, child } => {
                self.steps.get_mut(parent)?.children.get_mut(child)
            }
        }
    }

    /// The list `path` is an element of, so that reordering and deleting work
    /// the same at both levels.
    fn siblings_mut(&mut self, path: StepPath) -> Option<&mut Vec<StepRow>> {
        match path {
            StepPath::Top(_) => Some(&mut self.steps),
            StepPath::Child { parent, .. } => Some(&mut self.steps.get_mut(parent)?.children),
        }
    }

    /// Surfaces a failed background write in the form's error line rather than
    /// leaving the tree showing something the file does not say.
    fn report_write_failure(
        &self,
        write: Task<anyhow::Result<()>>,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{what}できませんでした: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Points the form's folder field at `folder` without that counting as an
    /// unsaved edit, for when the tree moved or renamed the macro on show.
    fn set_form_folder(
        &mut self,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = folder.unwrap_or_default();
        self.folder.update(cx, |editor, cx| {
            editor.set_text(text.clone(), window, cx);
        });
        if let Some(snapshot) = &mut self.saved_snapshot {
            snapshot.folder = text;
        }
    }

    fn begin_folder_rename(&mut self, folder: String, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text(folder.clone(), window, cx);
            editor.select_all(&Default::default(), window, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);
        self.renaming_folder = Some((folder, editor));
        self.error = None;
        cx.notify();
    }

    /// Renames a folder by rewriting the `folder` of every macro in it, which
    /// is all a folder is: the macros stay a flat array on disk.
    fn confirm_folder_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, field)) = self.renaming_folder.take() else {
            return;
        };
        let Some(renamed) = normalize_folder(&field.read(cx).text(cx)) else {
            self.error = Some("フォルダー名を入力してください。".into());
            self.renaming_folder = Some((from, field));
            cx.notify();
            return;
        };
        if renamed == from {
            cx.notify();
            return;
        }
        // Writing before the first read would rewrite the file from an empty
        // in-memory copy, dropping every macro and connection already in it.
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            self.renaming_folder = Some((from, field));
            cx.notify();
            return;
        }

        let mut file = self.store.read(cx).file().clone();
        let mut moved = 0usize;
        for terminal_macro in &mut file.macros {
            let Some(folder) = terminal_macro.folder.as_deref() else {
                continue;
            };
            if let Some(folder) = folder_after_rename(folder, &from, &renamed) {
                terminal_macro.folder = Some(folder);
                moved += 1;
            }
        }
        if moved == 0 {
            cx.notify();
            return;
        }

        self.collapsed_folders = std::mem::take(&mut self.collapsed_folders)
            .into_iter()
            .map(|collapsed| {
                collapsed
                    .map(|folder| folder_after_rename(&folder, &from, &renamed).unwrap_or(folder))
            })
            .collect();
        let form_folder = normalize_folder(&self.folder.read(cx).text(cx));
        if let Some(folder) = form_folder
            && let Some(folder) = folder_after_rename(&folder, &from, &renamed)
        {
            self.set_form_folder(Some(folder), window, cx);
        }
        self.error = None;

        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダー名を変更", cx);
        cx.notify();
    }

    /// Files `name` under `folder` (`None` for [`UNFILED_FOLDER`]), which is
    /// what a drag from the tree onto a heading does.
    fn move_macro_to_folder(
        &mut self,
        name: &str,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }

        let mut file = self.store.read(cx).file().clone();
        let Some(moved) = file
            .macros
            .iter_mut()
            .find(|candidate| candidate.name == name)
        else {
            return;
        };
        if moved.folder == folder {
            return;
        }
        moved.folder = folder.clone();

        if self.original_name.as_deref() == Some(name) {
            self.set_form_folder(folder, window, cx);
        }
        self.error = None;

        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダーを移動", cx);
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        // Writing before the first read would rewrite the file from an empty
        // in-memory copy, dropping every macro and connection already in it.
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }

        let name = self.name.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            self.error = Some("マクロ名を入力してください。".into());
            cx.notify();
            return;
        }

        let mut steps = Vec::with_capacity(self.steps.len());
        for (index, row) in self.steps.iter().enumerate() {
            match row.to_step(&(index + 1).to_string(), cx) {
                Ok(step) => steps.push(step),
                Err(message) => {
                    self.error = Some(message.into());
                    cx.notify();
                    return;
                }
            }
        }

        let Ok(repeat) = self.repeat.read(cx).text(cx).trim().parse::<u32>() else {
            self.error = Some("繰り返し回数は 0 以上の整数で入力してください。".into());
            cx.notify();
            return;
        };
        let Some(repeat_interval_ms) = parse_seconds(&self.repeat_interval.read(cx).text(cx))
        else {
            self.error = Some("繰り返しの間隔は 0 以上の秒数で入力してください。".into());
            cx.notify();
            return;
        };

        let terminal_macro = TerminalMacro {
            name: name.clone(),
            folder: normalize_folder(&self.folder.read(cx).text(cx)),
            tags: normalize_tags(&self.tags.read(cx).text(cx)),
            description: self.description.read(cx).text(cx).trim().to_string(),
            repeat,
            repeat_interval_ms,
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

    fn request_delete(&mut self, name: String, cx: &mut Context<Self>) {
        self.pending_delete = Some(name);
        self.error = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.pending_delete.take() else {
            return;
        };
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }

        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            file.macros.retain(|candidate| candidate.name != name);
            store.write(file, cx)
        });
        // The form was showing the macro that just went away, so put it back to
        // a blank new macro rather than leaving a rename of a deleted entry.
        if self.original_name.as_deref() == Some(name.as_str()) {
            self.load_macro(None, window, cx);
        }
        cx.notify();

        self.report_write_failure(write, "削除", cx);
    }

    /// The left pane: the saved macros grouped by folder, with the folder
    /// headings collapsible, renamable and droppable.
    fn render_macro_tree(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if !self.store.read(cx).is_loaded() {
            return Label::new("読み込み中…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }

        let rows = self.macro_tree(cx);
        if rows.is_empty() {
            let filtered = !self.list_filter.read(cx).text(cx).trim().is_empty();
            return Label::new(if filtered {
                "絞り込みに一致するマクロがありません"
            } else {
                "保存済みのマクロがありません"
            })
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element();
        }

        let editing = self.original_name.clone();
        let renaming = self
            .renaming_folder
            .as_ref()
            .map(|(folder, field)| (folder.clone(), field.clone()));
        let mut tree = v_flex()
            .id("saved-macros")
            .size_full()
            .overflow_y_scroll()
            .gap_0p5();

        for (index, row) in rows.into_iter().enumerate() {
            match row {
                MacroTreeRow::Folder {
                    folder,
                    collapsed,
                    count,
                } => {
                    let renamed_field = match (&renaming, &folder) {
                        (Some((renamed, field)), Some(folder)) if renamed == folder => {
                            Some(field.clone())
                        }
                        _ => None,
                    };
                    if let Some(field) = renamed_field {
                        tree = tree.child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .child(
                                    input_frame_sized(
                                        InputFrameSize::Compact,
                                        field.focus_handle(cx).contains_focused(window, cx),
                                        cx,
                                    )
                                    .flex_1()
                                    .min_w_0()
                                    .child(field.clone()),
                                )
                                .child(
                                    IconButton::new(
                                        ("confirm-folder-rename", index),
                                        IconName::Check,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("この名前に変更します"))
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            this.confirm_folder_rename(window, cx);
                                        },
                                    )),
                                )
                                .child(
                                    IconButton::new(
                                        ("cancel-folder-rename", index),
                                        IconName::Close,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("名前の変更をやめます"))
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.renaming_folder = None;
                                            cx.notify();
                                        },
                                    )),
                                ),
                        );
                        continue;
                    }

                    let heading = folder.clone().unwrap_or_else(|| UNFILED_FOLDER.to_string());
                    tree = tree.child(
                        h_flex()
                            .id(("macro-folder", index))
                            .w_full()
                            .gap_1()
                            .rounded_sm()
                            .drag_over::<DraggedMacro>(|style, _, _, cx| {
                                style.bg(cx.theme().colors().drop_target_background)
                            })
                            .on_drop(cx.listener({
                                let folder = folder.clone();
                                move |this, dragged: &DraggedMacro, window, cx| {
                                    this.move_macro_to_folder(
                                        &dragged.name,
                                        folder.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(
                                Disclosure::new(("macro-folder-toggle", index), !collapsed)
                                    .on_click(cx.listener({
                                        let folder = folder.clone();
                                        move |this, _, _, cx| {
                                            if !this.collapsed_folders.remove(&folder) {
                                                this.collapsed_folders.insert(folder.clone());
                                            }
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                h_flex()
                                    .id(("macro-folder-label", index))
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .cursor_pointer()
                                    .on_click(cx.listener({
                                        let folder = folder.clone();
                                        move |this, _, _, cx| {
                                            if !this.collapsed_folders.remove(&folder) {
                                                this.collapsed_folders.insert(folder.clone());
                                            }
                                            cx.notify();
                                        }
                                    }))
                                    .child(
                                        Icon::new(IconName::Folder)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new(heading)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                    .child(
                                        Label::new(format!("({count})"))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .when_some(folder, |this, folder| {
                                this.child(
                                    IconButton::new(("rename-folder", index), IconName::Pencil)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text(
                                            "フォルダー名を変更します (中のマクロをまとめて移動します)",
                                        ))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.begin_folder_rename(folder.clone(), window, cx);
                                        })),
                                )
                            }),
                    );
                }
                MacroTreeRow::Macro(terminal_macro) => {
                    let name = terminal_macro.name.clone();
                    let selected = editing.as_deref() == Some(name.as_str());
                    let detail = format!("{} ステップ", terminal_macro.steps.len());
                    tree = tree.child(
                        div()
                            .id(("saved-macro-row", index))
                            .w_full()
                            .on_drag(DraggedMacro { name: name.clone() }, |dragged, _, _, cx| {
                                let label = SharedString::from(dragged.name.clone());
                                cx.new(|_| DraggedMacroLabel(label))
                            })
                            .child(
                                ListItem::new(("saved-macro", index))
                                    .spacing(ListItemSpacing::Sparse)
                                    .toggle_state(selected)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.select_macro(Some(terminal_macro.clone()), window, cx);
                                    }))
                                    .child(Label::new(name).truncate())
                                    .end_slot(
                                        Label::new(detail)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            ),
                    );
                }
            }
        }

        tree.into_any_element()
    }

    /// Dropdown of the folders already in use, so a macro can be filed under an
    /// existing folder without retyping (and mistyping) its path.
    fn render_folder_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folders = self.known_folders(cx);
        let view = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.entry("(フォルダーなし)", None, {
                let view = view.clone();
                move |window, cx| {
                    view.update(cx, |this, cx| {
                        this.folder.update(cx, |editor, cx| {
                            editor.set_text("", window, cx);
                        });
                        cx.notify();
                    })
                    .ok();
                }
            });
            for folder in &folders {
                let folder = folder.clone();
                let view = view.clone();
                menu = menu.entry(folder.clone(), None, move |window, cx| {
                    let folder = folder.clone();
                    view.update(cx, |this, cx| {
                        this.folder.update(cx, |editor, cx| {
                            editor.set_text(folder, window, cx);
                        });
                        cx.notify();
                    })
                    .ok();
                });
            }
            menu
        });

        DropdownMenu::new("macro-folder-choices", "既存のフォルダー", menu)
            .style(DropdownStyle::Outlined)
    }

    fn render_step(
        &self,
        path: StepPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.row(path) else {
            return div().into_any_element();
        };
        let kind = row.kind;
        let element_id = row.element_id();
        let index = path.index();
        let sibling_count = match path {
            StepPath::Top(_) => self.steps.len(),
            StepPath::Child { parent, .. } => self
                .steps
                .get(parent)
                .map(|parent_row| parent_row.children.len())
                .unwrap_or(0),
        };
        let nested = matches!(path, StepPath::Child { .. });
        // A 繰り返し two levels deep is only editable in the JSON file, so its
        // row shows what it is and leaves the value untouched.
        let read_only = row.preserved.is_some();

        let view = cx.weak_entity();
        let kinds: &'static [StepKind] = if nested {
            &StepKind::NESTABLE
        } else {
            &StepKind::ALL
        };
        let kind_menu = ContextMenu::build(window, cx, {
            move |mut menu, _, _| {
                for candidate in kinds.iter().copied() {
                    let view = view.clone();
                    menu = menu.toggleable_entry(
                        candidate.label(),
                        candidate == kind,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            view.update(cx, |this, cx| {
                                let Some(row) = this.row_mut(path) else {
                                    return;
                                };
                                row.kind = candidate;
                                // Switching into a step that needs a duration
                                // would otherwise leave the field blank and
                                // fail validation on save.
                                if candidate.has_duration()
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

        let children = (kind == StepKind::Repeat && !read_only).then(|| {
            let child_rows = (0..row.children.len())
                .map(|child| {
                    self.render_step(
                        StepPath::Child {
                            parent: index,
                            child,
                        },
                        window,
                        cx,
                    )
                })
                .collect::<Vec<_>>();
            v_flex().w_full().gap_1().pl_4().children(child_rows).child(
                Button::new(("add-child-step", element_id), "子ステップを追加")
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_child_step(index, StepKind::Send, window, cx);
                    })),
            )
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
                                Label::new(path.label())
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(if read_only {
                                Label::new("入れ子の繰り返し (JSON で編集してください)")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .into_any_element()
                            } else {
                                DropdownMenu::new(
                                    ("step-kind", element_id),
                                    kind.label(),
                                    kind_menu,
                                )
                                .style(DropdownStyle::Outlined)
                                .into_any_element()
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                IconButton::new(("step-up", element_id), IconName::ArrowUp)
                                    .icon_size(IconSize::Small)
                                    .disabled(index == 0)
                                    .tooltip(Tooltip::text("ひとつ上へ移動します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let Some(siblings) = this.siblings_mut(path) else {
                                            return;
                                        };
                                        if index > 0 && index < siblings.len() {
                                            siblings.swap(index, index - 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                IconButton::new(("step-down", element_id), IconName::ArrowDown)
                                    .icon_size(IconSize::Small)
                                    .disabled(index + 1 >= sibling_count)
                                    .tooltip(Tooltip::text("ひとつ下へ移動します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let Some(siblings) = this.siblings_mut(path) else {
                                            return;
                                        };
                                        if index + 1 < siblings.len() {
                                            siblings.swap(index, index + 1);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                IconButton::new(("step-delete", element_id), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("このステップを削除します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let Some(siblings) = this.siblings_mut(path) else {
                                            return;
                                        };
                                        if index < siblings.len() {
                                            siblings.remove(index);
                                            cx.notify();
                                        }
                                    })),
                            ),
                    ),
            )
            .when(kind.has_text() && !read_only, |this| {
                this.child(
                    input_frame_sized(
                        InputFrameSize::Compact,
                        row.text.focus_handle(cx).contains_focused(window, cx),
                        cx,
                    )
                    .child(row.text.clone()),
                )
            })
            .when(kind == StepKind::Send && !read_only, |this| {
                this.child(
                    Checkbox::new(("step-newline", element_id), row.newline.into())
                        .label("末尾で Enter を送る")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                            let checked = *state == ToggleState::Selected;
                            if let Some(row) = this.row_mut(path)
                                && row.newline != checked
                            {
                                row.newline = checked;
                                cx.notify();
                            }
                        })),
                )
            })
            .when(kind == StepKind::Repeat && !read_only, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("繰り返し回数")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            input_frame_sized(
                                InputFrameSize::Compact,
                                row.count.focus_handle(cx).contains_focused(window, cx),
                                cx,
                            )
                            .w(rems(4.))
                            .child(row.count.clone()),
                        )
                        .child(
                            Label::new("(0 回は中止まで繰り返します)")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(kind.has_duration() && !read_only, |this| {
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
                        .child(
                            input_frame_sized(
                                InputFrameSize::Compact,
                                row.number.focus_handle(cx).contains_focused(window, cx),
                                cx,
                            )
                            .w(rems(6.))
                            .child(row.number.clone()),
                        )
                        .when(kind == StepKind::WaitFor, |this| {
                            this.child(
                                Checkbox::new(("step-regex", element_id), row.regex.into())
                                    .label("正規表現")
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(
                                        move |this, state: &ToggleState, _, cx| {
                                            let checked = *state == ToggleState::Selected;
                                            if let Some(row) = this.row_mut(path)
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
            .children(children)
            .into_any_element()
    }
}

impl Render for MacroEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steps = (0..self.steps.len())
            .map(|index| self.render_step(StepPath::Top(index), window, cx))
            .collect::<Vec<_>>();
        let tree = self.render_macro_tree(window, cx);
        let listed = self.listed_macros(cx).len();
        let total = self.store.read(cx).macros().len();
        let tree_header = if listed == total {
            format!("保存済みのマクロ ({total} 件)")
        } else {
            format!("保存済みのマクロ ({listed} / {total} 件)")
        };
        let form_header = match &self.original_name {
            Some(name) => format!("『{name}』を編集"),
            None => "新規マクロ".to_string(),
        };
        let editing = self.original_name.clone();
        let pending_delete = self.pending_delete.clone();
        let pending_selection = self.pending_selection.clone();
        let folder_menu = self.render_folder_menu(window, cx).into_any_element();
        let field = |editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>| {
            input_frame(editor.focus_handle(cx).contains_focused(window, cx), cx)
                .child(editor.clone())
        };
        let list_filter_field = field(&self.list_filter, window, cx);
        let name_field = field(&self.name, window, cx);
        let folder_field = field(&self.folder, window, cx);
        let tags_field = field(&self.tags, window, cx);
        let description_field = field(&self.description, window, cx);
        let repeat_field = field(&self.repeat, window, cx).w(rems(4.));
        let repeat_interval_field = field(&self.repeat_interval, window, cx).w(rems(4.));
        let viewport = window.viewport_size();

        v_flex()
            // The two panes need a bounded box: the tree and the form each
            // scroll inside it rather than growing the modal off screen.
            .w(rems(60.))
            .h(rems(38.))
            .max_w(viewport.width - px(64.))
            .max_h(viewport.height - px(96.))
            .overflow_hidden()
            .elevation_3(cx)
            .key_context("TerminalMacroEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| cx.emit(DismissEvent)))
            .child(
                Modal::new("terminal-macro-editor", None)
                    .header(ModalHeader::new().headline("マクロの編集"))
                    .child(
                        h_flex()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .px(DynamicSpacing::Base12.rems(cx))
                            .gap_3()
                            .child(
                                v_flex()
                                    .w(rems(17.))
                                    .flex_none()
                                    .h_full()
                                    .min_h_0()
                                    .gap_1()
                                    .pr_3()
                                    .border_r_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_1()
                                            .justify_between()
                                            .child(
                                                Label::new(tree_header)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                            )
                                            .child(
                                                Button::new("new-macro", "新規")
                                                    .label_size(LabelSize::Small)
                                                    .start_icon(Icon::new(IconName::Plus))
                                                    .tooltip(Tooltip::text(
                                                        "空のマクロを右側に開きます",
                                                    ))
                                                    .on_click(cx.listener(
                                                        |this, _, window, cx| {
                                                            this.select_macro(None, window, cx);
                                                        },
                                                    )),
                                            ),
                                    )
                                    .child(list_filter_field)
                                    .child(v_flex().flex_1().min_h_0().child(tree)),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .min_h_0()
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_1()
                                            .justify_between()
                                            .child(
                                                Label::new(form_header)
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .truncate(),
                                            )
                                            .when_some(editing, |this, name| {
                                                this.child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new("duplicate-macro", "複製")
                                                                .label_size(LabelSize::Small)
                                                                .start_icon(Icon::new(
                                                                    IconName::Copy,
                                                                ))
                                                                .tooltip(Tooltip::text(
                                                                    "同じ内容の新しいマクロとして開きます",
                                                                ))
                                                                .on_click(cx.listener(
                                                                    |this, _, window, cx| {
                                                                        this.duplicate_macro(
                                                                            window, cx,
                                                                        );
                                                                    },
                                                                )),
                                                        )
                                                        .child(
                                                            Button::new("delete-macro", "削除")
                                                                .label_size(LabelSize::Small)
                                                                .color(Color::Error)
                                                                .start_icon(Icon::new(
                                                                    IconName::Trash,
                                                                ))
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this.request_delete(
                                                                            name.clone(),
                                                                            cx,
                                                                        );
                                                                    },
                                                                )),
                                                        ),
                                                )
                                            }),
                                    )
                                    .child(
                                        v_flex()
                                            .id("macro-form")
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_y_scroll()
                                            .gap_2()
                                            .pr_1()
                                            .child(name_field)
                                            .child(
                                                h_flex()
                                                    .gap_1()
                                                    .child(folder_field.flex_1())
                                                    .child(folder_menu),
                                            )
                                            .child(tags_field)
                                            .child(description_field)
                                            .child(
                                                h_flex()
                                                    .gap_2()
                                                    .child(
                                                        Label::new("繰り返し回数")
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .child(repeat_field)
                                                    .child(
                                                        Label::new("間隔 (秒)")
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .child(repeat_interval_field)
                                                    .child(
                                                        Label::new("(0 回は中止まで繰り返します)")
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    ),
                                            )
                                            .children(steps)
                                            .child(
                                                h_flex().gap_1().child(
                                                    Button::new("add-step", "ステップを追加")
                                                        .start_icon(Icon::new(IconName::Plus))
                                                        .on_click(cx.listener(
                                                            |this, _, window, cx| {
                                                                this.add_step(
                                                                    StepKind::Send,
                                                                    window,
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                ),
                                            ),
                                    )
                                    .when_some(pending_selection, |this, edited| {
                                        let prompt = match &edited {
                                            Some(edited) => format!(
                                                "編集中の内容は保存されていません。破棄して『{}』を開きますか？",
                                                edited.name
                                            ),
                                            None => "編集中の内容は保存されていません。破棄して新規マクロを開きますか？"
                                                .to_string(),
                                        };
                                        this.child(
                                            v_flex()
                                                .w_full()
                                                .gap_1()
                                                .child(
                                                    Label::new(prompt)
                                                        .size(LabelSize::Small)
                                                        .color(Color::Warning),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new(
                                                                "confirm-selection",
                                                                "破棄して切り替え",
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.confirm_selection(
                                                                        window, cx,
                                                                    );
                                                                },
                                                            )),
                                                        )
                                                        .child(
                                                            Button::new(
                                                                "cancel-selection",
                                                                "編集を続ける",
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.pending_selection = None;
                                                                    cx.notify();
                                                                },
                                                            )),
                                                        ),
                                                ),
                                        )
                                    })
                                    .when_some(pending_delete, |this, name| {
                                        this.child(
                                            h_flex()
                                                .w_full()
                                                .gap_1()
                                                .justify_between()
                                                .child(
                                                    Label::new(format!(
                                                        "『{name}』を削除しますか？"
                                                    ))
                                                    .size(LabelSize::Small),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new("confirm-delete", "削除")
                                                                .color(Color::Error)
                                                                .on_click(cx.listener(
                                                                    |this, _, window, cx| {
                                                                        this.confirm_delete(
                                                                            window, cx,
                                                                        );
                                                                    },
                                                                )),
                                                        )
                                                        .child(
                                                            Button::new(
                                                                "cancel-delete",
                                                                "キャンセル",
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.pending_delete = None;
                                                                    cx.notify();
                                                                },
                                                            )),
                                                        ),
                                                ),
                                        )
                                    })
                                    .when_some(self.error.clone(), |this, error| {
                                        this.child(
                                            Label::new(error)
                                                .size(LabelSize::Small)
                                                .color(Color::Error),
                                        )
                                    }),
                            ),
                    )
                    .footer(
                        ModalFooter::new().end_slot(
                            h_flex()
                                .gap_1()
                                .child(Button::new("cancel", "キャンセル").on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                ))
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
    // The modal layer focuses whatever this returns once the modal is shown, so
    // handing it the name field is what makes the form typable on open.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
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
    folder: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    /// Masked, and never filled in from the store: a remembered password is
    /// read straight out of the credential store when connecting, so the
    /// plaintext never has to come back to the screen.
    password: Entity<Editor>,
    protocol: ConnectionProtocol,
    macro_name: Option<String>,
    log: bool,
    remember_password: bool,
    credentials: QuickConnectCredentials,
    /// Credential URLs (see [`quick_connect_credentials::credential_url`])
    /// known to hold a password, for the 「保存済み」 badge.
    stored_passwords: HashSet<String>,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _store_loaded: Subscription,
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
        let folder = single_line("フォルダー (例: 拠点/本社。省略可)", window, cx);
        let host = single_line("ホスト名 / IP アドレス", window, cx);
        let port = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("22", window, cx);
            editor
        });
        let user = single_line("ユーザー名 (SSH)", window, cx);
        let password = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("パスワード (省略可)", window, cx);
            editor.set_masked(true, cx);
            editor
        });

        let log = TerminalSettings::get_global(cx).session_log.auto;
        let focus_handle = cx.focus_handle();
        window.focus(&host.focus_handle(cx), cx);
        // The saved-connection list and the macro dropdown both read the store
        // on every render, and the file lands asynchronously. The badges follow
        // the same file, so they are refreshed from here too.
        let store_loaded = cx.observe(&store, |this, _, cx| {
            this.refresh_stored_passwords(cx);
            cx.notify();
        });

        Self {
            store,
            workspace,
            name,
            folder,
            host,
            port,
            user,
            password,
            protocol: ConnectionProtocol::Ssh,
            macro_name: None,
            log,
            remember_password: false,
            credentials: QuickConnectCredentials::new(zed_credentials_provider::global(cx)),
            stored_passwords: HashSet::default(),
            error: None,
            focus_handle,
            _store_loaded: store_loaded,
        }
    }

    /// Re-reads which saved connections have a remembered password. Runs on
    /// every store change rather than on a timer, because the only things that
    /// add or remove one also write the store.
    fn refresh_stored_passwords(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_loaded() {
            return;
        }
        let connections = self.store.read(cx).connections().to_vec();
        let credentials = self.credentials.clone();
        cx.spawn(async move |this, cx| {
            let mut stored = HashSet::default();
            for connection in connections {
                if credentials.has_password(&connection, cx).await {
                    stored.insert(quick_connect_credentials::credential_url(&connection));
                }
            }
            this.update(cx, |this, cx| {
                if this.stored_passwords != stored {
                    this.stored_passwords = stored;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn has_stored_password(&self, connection: &SavedConnection) -> bool {
        self.stored_passwords
            .contains(&quick_connect_credentials::credential_url(connection))
    }

    /// Drops the password stored for `connection`, then refreshes the badges.
    fn forget_password(&mut self, connection: SavedConnection, cx: &mut Context<Self>) {
        let credentials = self.credentials.clone();
        cx.spawn(async move |this, cx| {
            let result = credentials.forget(&connection, cx).await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.error = Some(format!("{error}").into());
                }
                this.refresh_stored_passwords(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        let name = if name.is_empty() { host.clone() } else { name };
        // The file transfer protocol and port are part of the same entry
        // (ADR-F28) but are chosen on the transfer screen, so existing ones are
        // carried over rather than dropped when this form saves over the entry.
        let existing = self
            .store
            .read(cx)
            .connections()
            .iter()
            .find(|candidate| candidate.name == name)
            .cloned();
        let transfer_protocol = existing
            .as_ref()
            .and_then(|existing| existing.transfer_protocol.clone());
        let transfer_port = existing.and_then(|existing| existing.transfer_port);

        Ok(SavedConnection {
            name,
            folder: normalize_folder(&self.folder.read(cx).text(cx)),
            protocol: self.protocol,
            host,
            port,
            user: (!user.is_empty()).then_some(user),
            macro_name: self.macro_name.clone(),
            log: Some(self.log),
            remember_password: self.remember_password,
            transfer_protocol,
            transfer_port,
        })
    }

    /// What the password field holds right now. Not trimmed: leading and
    /// trailing spaces are part of a password.
    fn typed_password(&self, cx: &App) -> Option<String> {
        let password = self.password.read(cx).text(cx);
        (!password.is_empty()).then_some(password)
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
        self.remember_password = connection.remember_password;
        // A remembered password stays in the credential store; loading an entry
        // must not put the previous entry's password back in the field either.
        self.password
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.name.update(cx, |editor, cx| {
            editor.set_text(connection.name.clone(), window, cx)
        });
        self.folder.update(cx, |editor, cx| {
            editor.set_text(connection.folder.clone().unwrap_or_default(), window, cx)
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
        // See `MacroEditor::save`: writing before the first read truncates the
        // file to whatever little the store happens to hold.
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
        let connection = match self.form_connection(cx) {
            Ok(connection) => connection,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        let typed_password = self.typed_password(cx);
        let credentials = self.credentials.clone();
        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            match file
                .connections
                .iter()
                .position(|candidate| candidate.name == connection.name)
            {
                Some(index) => file.connections[index] = connection.clone(),
                None => file.connections.push(connection.clone()),
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
                return;
            }
            // The password is the one thing that does not go into the file.
            // Clearing 「パスワードを記憶する」 and saving is how the user drops
            // one they no longer want kept.
            let stored = match (connection.remember_password, typed_password) {
                (true, Some(password)) => credentials.store(&connection, &password, cx).await,
                // An empty field with the box still ticked keeps whatever is
                // already stored, so the user can edit the port without having
                // to retype the password.
                (true, None) => Ok(()),
                (false, _) => credentials.forget(&connection, cx).await,
            };
            this.update(cx, |this, cx| {
                if let Err(error) = stored {
                    this.error = Some(format!("{error}").into());
                }
                this.refresh_stored_passwords(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn delete_connection(&mut self, connection: SavedConnection, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
        let name = connection.name.clone();
        let credentials = self.credentials.clone();
        let url = quick_connect_credentials::credential_url(&connection);
        let (write, password_still_used) = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            file.connections.retain(|candidate| candidate.name != name);
            // Credentials are keyed by target, not by entry name, so a second
            // entry for the same host:port is still using this password.
            let still_used = file
                .connections
                .iter()
                .any(|candidate| quick_connect_credentials::credential_url(candidate) == url);
            (store.write(file, cx), still_used)
        });
        cx.spawn(async move |this, cx| {
            write.await.log_err();
            // Deleting the entry takes its password with it; otherwise a later
            // entry for the same host would silently inherit it.
            if !password_still_used {
                credentials.forget(&connection, cx).await.log_err();
            }
            this.update(cx, |this, cx| this.refresh_stored_passwords(cx))
                .ok();
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

        // A password typed here is used for this connection whether or not it
        // is remembered; remembering only decides if it is also written to the
        // credential store. Encrypting it up front keeps the plaintext out of
        // the task that runs after the modal is gone.
        let typed_password = self.typed_password(cx);
        let password = typed_password
            .as_deref()
            .and_then(|password| StoredPassword::try_from(password).log_err());
        if self.remember_password
            && let Some(typed_password) = typed_password
        {
            let credentials = self.credentials.clone();
            let connection = connection.clone();
            cx.spawn(async move |_, cx| {
                credentials
                    .store(&connection, &typed_password, cx)
                    .await
                    .log_err();
            })
            .detach();
        }

        cx.emit(DismissEvent);
        connect_and_run_with_password(
            workspace,
            connection,
            terminal_macro,
            start_log,
            password,
            window,
            cx,
        );
    }

    /// Hands the form's host over to the file transfer screen (ADR-F28)
    /// without connecting a terminal first. The handler lives in the
    /// `file_transfer` crate, so the values travel in the action.
    fn open_file_transfer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let connection = match self.form_connection(cx) {
            Ok(connection) => connection,
            Err(message) => {
                self.error = Some(message);
                cx.notify();
                return;
            }
        };
        let action = zed_actions::terminal::OpenFileTransfer {
            host: Some(connection.host.clone()),
            port: match connection.protocol {
                ConnectionProtocol::Ssh => Some(connection.port_or_default()),
                // Telnet's port says nothing about the FTP service that would
                // carry the files, so the FTP default is left to stand.
                ConnectionProtocol::Telnet => None,
            },
            user: connection.user.clone(),
            protocol: Some(
                match connection.protocol {
                    ConnectionProtocol::Ssh => "sftp",
                    ConnectionProtocol::Telnet => "ftp",
                }
                .to_string(),
            ),
        };
        cx.emit(DismissEvent);
        // Deferred so this modal has left the modal layer's single slot before
        // the transfer screen asks for it.
        window.defer(cx, move |window, cx| {
            window.dispatch_action(Box::new(action), cx);
        });
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
    connect_and_run_with_password(
        workspace,
        connection,
        terminal_macro,
        start_log,
        None,
        window,
        cx,
    );
}

/// [`connect_and_run`] with a password that has not been stored yet — what
/// 「接続」 uses for a password typed into the modal. When `password` is `None`
/// and the connection remembers one, it is read from the credential store
/// instead (ADR-F28).
pub fn connect_and_run_with_password(
    workspace: WeakEntity<Workspace>,
    connection: SavedConnection,
    terminal_macro: Option<TerminalMacro>,
    start_log: bool,
    password: Option<StoredPassword>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace_entity) = workspace.upgrade() else {
        return;
    };
    workspace_entity.update(cx, |workspace, cx| {
        // リモートプロジェクトの端末はリモートホストで動くので、この PC の askpass
        // ソケット (`SSH_ASKPASS` / `ZED_ASKPASS_SOCKET`) は届かない。そのまま渡すと
        // `SSH_ASKPASS_REQUIRE=force` の ssh が問い合わせ先を失って失敗するため、
        // リモートでは保存パスワードを使わず端末内のプロンプトに任せる。
        let remote_project = workspace.project().read(cx).is_via_remote_server();
        cx.spawn_in(window, async move |workspace, cx| {
            let (program, mut args) = connection.command();
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

            let password = match password {
                Some(password) => Some(password),
                None if connection.remember_password => {
                    match cx.update(|_, cx| zed_credentials_provider::global(cx)) {
                        Ok(provider) => {
                            QuickConnectCredentials::new(provider)
                                .lookup(&connection, cx)
                                .await
                        }
                        // The window went away while we were looking up the
                        // program; there is nothing left to connect to.
                        Err(_) => return,
                    }
                }
                None => None,
            };

            // SSH takes the password out of band over an askpass socket, so it
            // never travels through the PTY. Telnet has no such door and is
            // answered after the terminal is up.
            let askpass = match (&password, connection.protocol) {
                (Some(password), ConnectionProtocol::Ssh) if !remote_project => {
                    let executor = cx.background_executor().clone();
                    match quick_connect_credentials::stored_password_proxy(
                        password.clone(),
                        executor,
                    )
                    .await
                    {
                        Ok(askpass) => Some(askpass),
                        Err(error) => {
                            log::warn!("askpass の準備に失敗しました: {error:#}");
                            None
                        }
                    }
                }
                _ => None,
            };
            let mut env = collections::HashMap::default();
            if let Some(askpass) = &askpass {
                env.extend(askpass.env.clone());
                let mut with_options = quick_connect_credentials::SSH_STORED_PASSWORD_ARGS
                    .iter()
                    .map(|option| option.to_string())
                    .collect::<Vec<_>>();
                with_options.append(&mut args);
                args = with_options;
            }

            let label = format!("{} ({})", connection.host, connection.protocol.label());
            let spawn_task = SpawnInTerminal {
                id: TaskId(format!("terminal::quick-connect::{}", connection.name)),
                full_label: label.clone(),
                label: connection.host.clone(),
                command: Some(program),
                args,
                command_label: label,
                env,
                use_new_terminal: true,
                allow_concurrent_runs: true,
                reveal: RevealStrategy::Always,
                reveal_target: RevealTarget::Dock,
                show_summary: false,
                show_command: false,
                show_rerun: true,
                ..SpawnInTerminal::default()
            };

            let Some(spawn) = spawn_task_in_terminal_panel(&workspace, &spawn_task, cx) else {
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

            match (askpass, password) {
                (Some(askpass), _) => {
                    let proxy = askpass.proxy;
                    terminal.update(cx, |terminal, _| terminal.hold_askpass_proxy(proxy));
                    report_askpass_outcomes(workspace.clone(), askpass.outcomes, cx);
                }
                (None, Some(password)) if connection.protocol == ConnectionProtocol::Telnet => {
                    quick_connect_credentials::answer_telnet_login(
                        &terminal,
                        connection.user.clone(),
                        password,
                        cx,
                    )
                    .detach();
                }
                (None, _) => {}
            }

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

/// Turns what the askpass proxy refused into a message the user can act on.
/// Ends by itself once the proxy is dropped with the terminal.
fn report_askpass_outcomes(
    workspace: WeakEntity<Workspace>,
    mut outcomes: futures::channel::mpsc::UnboundedReceiver<
        quick_connect_credentials::AskPassOutcome,
    >,
    cx: &mut AsyncWindowContext,
) {
    cx.spawn(async move |cx| {
        while let Some(outcome) = outcomes.next().await {
            let Some(message) = outcome.message() else {
                continue;
            };
            if workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(message.to_string(), cx)
                })
                .is_err()
            {
                return;
            }
        }
    })
    .detach();
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
            // Remembered so 「ファイル転送…」 can pre-fill this host without the
            // user retyping it (ADR-F28).
            terminal.update(cx, |terminal, _| {
                terminal.set_connection_origin(connection.clone());
            });

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
        if !self.store.read(cx).is_loaded() {
            return Label::new("読み込み中…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }
        let mut connections = self.store.read(cx).connections().to_vec();
        // Grouped by folder, and inside a folder left in the order the file
        // has them (`sort_by` is stable): that order is what the file transfer
        // screen's 接続先 picker lets the user rearrange (ADR-F28), so sorting
        // by name here would hide the result.
        connections.sort_by(|left, right| {
            left.folder
                .as_deref()
                .unwrap_or_default()
                .cmp(right.folder.as_deref().unwrap_or_default())
        });

        let mut list = v_flex().id("saved-connections").gap_0p5();
        let mut current_folder: Option<String> = None;
        for (index, connection) in connections.into_iter().enumerate() {
            if current_folder != connection.folder {
                current_folder = connection.folder.clone();
                if let Some(folder) = current_folder.clone() {
                    list = list.child(
                        Label::new(folder)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
            }
            let detail = format!(
                "{} {}:{}",
                connection.protocol.label(),
                connection.host,
                connection.port_or_default()
            );
            let has_password = self.has_stored_password(&connection);
            let applied = connection.clone();
            let deleted = connection.clone();
            list = list.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .justify_between()
                    // The name takes whatever is left; without `min_w_0` a
                    // full-width button keeps its content width and pushes the
                    // host, badge, and buttons past the modal's edge.
                    .child(
                        div().flex_1().min_w_0().child(
                            Button::new(("saved-connection", index), connection.name.clone())
                                .full_width()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.apply_connection(&applied, window, cx);
                                })),
                        ),
                    )
                    .child(
                        Label::new(detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .when(has_password, |this| {
                        this.child(
                            Label::new("保存済み")
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(
                            IconButton::new(("forget-password", index), IconName::LockOff)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("この接続先の保存済みパスワードを消します"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.forget_password(connection.clone(), cx);
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("delete-connection", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(
                                "この接続先を削除します (保存済みパスワードも消えます)",
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_connection(deleted.clone(), cx);
                            })),
                    ),
            );
        }
        list.into_any_element()
    }
}

impl Render for QuickConnectModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros);
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
                // Same folder-then-name grouping as the macro picker, so a long
                // list stays navigable here too.
                let mut current_folder: Option<&str> = None;
                for terminal_macro in &macros {
                    let folder = terminal_macro.folder_display();
                    if current_folder != Some(folder) {
                        current_folder = Some(folder);
                        if !folder.is_empty() {
                            menu = menu.header(folder.to_string());
                        }
                    }
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
        let field = |editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>| {
            input_frame(editor.focus_handle(cx).contains_focused(window, cx), cx)
                .child(editor.clone())
        };
        let host_field = field(&self.host, window, cx);
        let port_field = field(&self.port, window, cx).w(rems(5.));
        let user_field = field(&self.user, window, cx);
        let password_field = field(&self.password, window, cx);
        let name_field = field(&self.name, window, cx);
        let folder_field = field(&self.folder, window, cx);
        // The user name is what the stored password is filed under, so telnet
        // needs the field visible as soon as one is being remembered.
        let show_user = protocol == ConnectionProtocol::Ssh || self.remember_password;

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
                                        .child(host_field.flex_1())
                                        .child(
                                            Label::new("ポート")
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(port_field),
                                )
                                .when(show_user, |this| this.child(user_field))
                                .child(password_field)
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
                                    h_flex()
                                        .gap_3()
                                        .child(
                                            Checkbox::new("quick-connect-log", self.log.into())
                                                .label("ログを記録する")
                                                .on_click(cx.listener(
                                                    |this, state: &ToggleState, _, cx| {
                                                        let checked =
                                                            *state == ToggleState::Selected;
                                                        if this.log != checked {
                                                            this.log = checked;
                                                            cx.notify();
                                                        }
                                                    },
                                                )),
                                        )
                                        .child(
                                            Checkbox::new(
                                                "quick-connect-remember-password",
                                                self.remember_password.into(),
                                            )
                                            .label("パスワードを記憶する")
                                            .tooltip(Tooltip::text(
                                                "パスワードは OS の資格情報マネージャーに保存され、\
                                                 terminal_sessions.json には書き込まれません",
                                            ))
                                            .on_click(cx.listener(
                                                |this, state: &ToggleState, _, cx| {
                                                    let checked = *state == ToggleState::Selected;
                                                    if this.remember_password != checked {
                                                        this.remember_password = checked;
                                                        cx.notify();
                                                    }
                                                },
                                            )),
                                        ),
                                )
                                .child(name_field)
                                .child(folder_field)
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
                            .start_slot(
                                h_flex()
                                    .gap_1()
                                    .child(Button::new("save-connection", "保存").on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.save_connection(cx);
                                        }),
                                    ))
                                    .child(
                                        Button::new("open-file-transfer", "ファイル転送を開く")
                                            .tooltip(Tooltip::text(
                                                "このホストと SFTP / FTP でファイルをやり取りします",
                                            ))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.open_file_transfer(window, cx);
                                            })),
                                    ),
                            )
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
    use std::{
        cell::Cell,
        path::{Path, PathBuf},
        rc::Rc,
    };

    use gpui::{TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use terminal::terminal_macro::parse_sessions_file;
    use util::path;
    use workspace::MultiWorkspace;

    use super::*;

    fn sessions_path() -> PathBuf {
        PathBuf::from(path!("/config/terminal_sessions.json"))
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    /// A workspace over a fake fs that already holds `sessions_json`, plus a
    /// store pointed at it. The store is deliberately *not* pumped: every test
    /// that wants the file read has to say so, because the whole point of these
    /// tests is what the UI does before and after that read lands.
    async fn init_workspace(
        cx: &mut TestAppContext,
        sessions_json: &str,
    ) -> (
        Arc<FakeFs>,
        Entity<Workspace>,
        gpui::WindowHandle<MultiWorkspace>,
    ) {
        init_test(cx);
        // `terminal.starts_open` defaults to true, so a panel added by a test
        // would open and spawn a shell of its own — a real PTY whose reader
        // thread then schedules onto the test's foreground executor.
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(false);
            });
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/config"),
            serde_json::json!({ "terminal_sessions.json": sessions_json }),
        )
        .await;
        let project = Project::test(fs.clone(), [], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the test window should have a workspace");
        (fs, workspace, window)
    }

    fn new_store(fs: &Arc<FakeFs>, cx: &mut VisualTestContext) -> Entity<TerminalSessionStore> {
        let fs = fs.clone();
        cx.update(|_, cx| cx.new(|cx| TerminalSessionStore::new(fs, sessions_path(), cx)))
    }

    fn open_editor(
        workspace: &Entity<Workspace>,
        store: &Entity<TerminalSessionStore>,
        cx: &mut VisualTestContext,
    ) -> Entity<MacroEditor> {
        let store = store.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |window, cx| {
                MacroEditor::new(store, None, window, cx)
            });
            workspace
                .active_modal::<MacroEditor>(cx)
                .expect("the editor should be the active modal")
        })
    }

    fn open_picker(
        workspace: &Entity<Workspace>,
        store: &Entity<TerminalSessionStore>,
        cx: &mut VisualTestContext,
    ) -> Entity<MacroPicker> {
        let store = store.clone();
        workspace.update_in(cx, |workspace, window, cx| {
            let handle = cx.weak_entity();
            workspace.toggle_modal(window, cx, |window, cx| {
                MacroPicker::new(store, handle, window, cx)
            });
            workspace
                .active_modal::<MacroPicker>(cx)
                .expect("the picker should be the active modal")
        })
    }

    /// The macro rows of the picker, folder headings left out.
    fn picker_names(picker: &Entity<MacroPicker>, cx: &mut VisualTestContext) -> Vec<String> {
        picker.update(cx, |picker, cx| {
            let picker = picker.picker.read(cx);
            picker
                .delegate
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MacroPickerEntry::Macro { index, .. } => {
                        Some(picker.delegate.macros.get(*index)?.name.clone())
                    }
                    MacroPickerEntry::Folder(_) => None,
                })
                .collect()
        })
    }

    /// Every row of the picker, with folder headings marked by a trailing `/`.
    fn picker_rows(picker: &Entity<MacroPicker>, cx: &mut VisualTestContext) -> Vec<String> {
        picker.update(cx, |picker, cx| {
            let picker = picker.picker.read(cx);
            picker
                .delegate
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MacroPickerEntry::Folder(folder) => Some(format!("{folder}/")),
                    MacroPickerEntry::Macro { index, .. } => {
                        Some(picker.delegate.macros.get(*index)?.name.clone())
                    }
                })
                .collect()
        })
    }

    fn set_picker_query(picker: &Entity<MacroPicker>, query: &str, cx: &mut VisualTestContext) {
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                picker.set_query(query, window, cx);
            });
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn the_editor_lists_the_macros_the_store_loads(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                // 保存済みのマクロ
                "macros": [
                    { "name": "ログイン", "steps": [{ "type": "send", "text": "enable" }] },
                    { "name": "設定保存", "steps": [] },
                    { "name": "再起動", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);

        let editor = open_editor(&workspace, &store, cx);

        // Opened before the file was read: the list has nothing to show yet.
        cx.update(|_, cx| {
            assert!(!store.read(cx).is_loaded());
            assert!(editor.read(cx).listed_macros(cx).is_empty());
        });

        let redrawn = Rc::new(Cell::new(false));
        let _observer = cx.update(|_, cx| {
            cx.observe(&editor, {
                let redrawn = redrawn.clone();
                move |_, _| redrawn.set(true)
            })
        });

        cx.run_until_parked();

        cx.update(|_, cx| {
            assert!(store.read(cx).is_loaded());
            let names = editor
                .read(cx)
                .listed_macros(cx)
                .into_iter()
                .map(|terminal_macro| terminal_macro.name)
                .collect::<Vec<_>>();
            // The list is sorted by folder then name, not by file order.
            assert_eq!(names, vec!["ログイン", "再起動", "設定保存"]);
        });
        assert!(
            redrawn.get(),
            "the editor must be re-rendered when the store finishes loading"
        );
    }

    #[gpui::test]
    async fn the_picker_lists_macros_that_arrive_after_it_opened(cx: &mut TestAppContext) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "ログイン", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);

        let picker = open_picker(&workspace, &store, cx);
        assert!(picker_names(&picker, cx).is_empty());

        cx.run_until_parked();
        assert_eq!(picker_names(&picker, cx), vec!["ログイン".to_string()]);
    }

    const FOLDERED_MACROS: &str = r#"{
        "macros": [
            { "name": "再起動", "folder": "拠点/大阪", "tags": ["ルーター"], "steps": [] },
            { "name": "設定保存", "folder": "拠点/本社", "steps": [] },
            { "name": "ログイン", "folder": "拠点/本社", "tags": ["ルーター", "定期"], "steps": [] },
            { "name": "メモ", "description": "フォルダーなし", "steps": [] }
        ]
    }"#;

    #[gpui::test]
    async fn the_picker_groups_macros_by_folder(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();

        // Un-foldered macros come first and get no heading; every folder is
        // introduced once, and the names inside it are sorted.
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "メモ",
                "拠点/大阪/",
                "再起動",
                "拠点/本社/",
                "ログイン",
                "設定保存",
            ]
        );

        // The headings must not be selectable, and the first selectable row is
        // the one the picker starts on.
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                assert_eq!(picker.delegate.selected_index(), 0);
                assert!(!picker.delegate.can_select(1, window, cx));
                assert!(picker.delegate.can_select(2, window, cx));
                assert_eq!(picker.delegate.separators_after_indices(), vec![0, 2]);
            });
        });
    }

    #[gpui::test]
    async fn the_picker_searches_folders_descriptions_and_tags(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();

        // A folder name on its own narrows to that folder.
        set_picker_query(&picker, "拠点/本社", cx);
        assert_eq!(
            picker_names(&picker, cx),
            vec!["ログイン".to_string(), "設定保存".to_string()]
        );

        // Tags are searchable, and the order is still folder then name.
        set_picker_query(&picker, "ルーター", cx);
        assert_eq!(
            picker_names(&picker, cx),
            vec!["再起動".to_string(), "ログイン".to_string()]
        );

        // So is the description.
        set_picker_query(&picker, "フォルダーなし", cx);
        assert_eq!(picker_names(&picker, cx), vec!["メモ".to_string()]);

        set_picker_query(&picker, "該当なし", cx);
        assert!(picker_names(&picker, cx).is_empty());
    }

    #[gpui::test]
    async fn the_picker_highlights_only_the_name(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "login", "folder": "site", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        // "site/login": the folder's own match positions must not bleed into
        // the name's highlight, and the name's are rebased onto it.
        set_picker_query(&picker, "sitelog", cx);

        picker.update(cx, |picker, cx| {
            let picker = picker.picker.read(cx);
            let highlights = picker
                .delegate
                .entries
                .iter()
                .find_map(|entry| match entry {
                    MacroPickerEntry::Macro {
                        name_highlights, ..
                    } => Some(name_highlights.clone()),
                    MacroPickerEntry::Folder(_) => None,
                })
                .expect("the macro should still be listed");
            assert_eq!(highlights, vec![0, 1, 2]);
        });
    }

    #[gpui::test]
    async fn editing_from_the_picker_swaps_in_the_editor(cx: &mut TestAppContext) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "ログイン", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(picker_names(&picker, cx), vec!["ログイン".to_string()]);

        // 「編集」 runs inside a `Picker` update. Swapping the modal from there
        // used to read the picker while it was leased, which panicked.
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                let edited = picker.delegate.selected_macro().cloned();
                assert!(edited.is_some(), "a macro should be selected");
                picker.delegate.open_editor(edited, window, cx);
            });
        });
        cx.run_until_parked();

        workspace.update(cx, |workspace, cx| {
            let editor = workspace
                .active_modal::<MacroEditor>(cx)
                .expect("the editor should have replaced the picker");
            assert_eq!(
                editor.read(cx).original_name.as_deref(),
                Some("ログイン"),
                "the editor should have loaded the selected macro"
            );
            assert!(
                workspace.active_modal::<MacroPicker>(cx).is_none(),
                "the picker should be gone"
            );
        });
    }

    #[gpui::test]
    async fn saving_writes_the_file_and_the_picker_follows(cx: &mut TestAppContext) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "既存", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("新しいマクロ", window, cx);
            });
            editor.add_step(StepKind::Send, window, cx);
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("show version", window, cx);
            });
            editor.save(cx);
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved
                .macros
                .iter()
                .map(|terminal_macro| terminal_macro.name.as_str())
                .collect::<Vec<_>>(),
            vec!["既存", "新しいマクロ"]
        );
        assert_eq!(
            saved.macros[1].steps,
            vec![MacroStep::Send {
                text: "show version".to_string(),
                newline: true
            }]
        );

        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.active_modal::<MacroEditor>(cx).is_none(),
                "a successful save should close the editor"
            );
        });

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_names(&picker, cx),
            vec!["新しいマクロ".to_string(), "既存".to_string()]
        );
    }

    #[gpui::test]
    async fn a_repeat_is_editable_one_level_deep_and_kept_verbatim_below_it(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let twice_nested = MacroStep::Repeat {
            count: 2,
            steps: vec![MacroStep::Clear],
        };
        let sent = MacroStep::Send {
            text: "show version".to_string(),
            newline: true,
        };
        let edited = TerminalMacro {
            name: "入れ子".to_string(),
            steps: vec![MacroStep::Repeat {
                count: 3,
                steps: vec![sent.clone(), twice_nested.clone()],
            }],
            ..TerminalMacro::default()
        };

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(edited), window, cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.steps.len(), 1, "the repeat is one top-level step");
            let children = &editor.steps[0].children;
            assert_eq!(children.len(), 2, "its steps become editable rows");
            assert!(children[0].preserved.is_none());
            assert_eq!(
                children[1].preserved.as_ref(),
                Some(&twice_nested),
                "a repeat two levels down is kept as it was read"
            );
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.add_child_step(0, StepKind::Sleep, window, cx);
            editor.save(cx);
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved.macros[0].steps,
            vec![MacroStep::Repeat {
                count: 3,
                steps: vec![sent, twice_nested, MacroStep::Sleep { ms: 1_000 }],
            }]
        );
    }

    #[gpui::test]
    async fn a_repeat_with_a_bad_count_names_the_step(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("繰り返し", window, cx);
            });
            editor.add_step(StepKind::Repeat, window, cx);
            editor.steps[0].count.update(cx, |count, cx| {
                count.set_text("なし", window, cx);
            });
            editor.save(cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("ステップ 1: 繰り返し回数は 0 以上の整数で入力してください。")
            );
        });
    }

    #[gpui::test]
    async fn saving_normalizes_the_folder_and_tags_and_keeps_the_old_entries(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "既存", "description": "旧形式", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("再起動", window, cx);
            });
            editor.folder.update(cx, |folder, cx| {
                folder.set_text(" 拠点 / 本社 / ", window, cx);
            });
            editor.tags.update(cx, |tags, cx| {
                tags.set_text(" ルーター , 定期 , ルーター ", window, cx);
            });
            editor.repeat.update(cx, |repeat, cx| {
                repeat.set_text("3", window, cx);
            });
            editor.repeat_interval.update(cx, |interval, cx| {
                interval.set_text("1.5", window, cx);
            });
            editor.add_step(StepKind::Clear, window, cx);
            editor.save(cx);
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(saved.macros.len(), 2, "the old macro must survive the save");
        let written = &saved.macros[1];
        assert_eq!(written.folder.as_deref(), Some("拠点/本社"));
        assert_eq!(
            written.tags,
            vec!["ルーター".to_string(), "定期".to_string()]
        );
        assert_eq!(written.repeat, 3);
        assert_eq!(written.repeat_interval_ms, 1_500);
        assert_eq!(written.steps, vec![MacroStep::Clear]);
        // The untouched macro keeps its shape, defaults and all.
        assert_eq!(saved.macros[0].folder, None);
        assert_eq!(saved.macros[0].repeat, 1);
    }

    #[gpui::test]
    async fn the_editor_list_filters_on_folder_and_tags(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let listed = |editor: &Entity<MacroEditor>, cx: &mut VisualTestContext| {
            editor.update(cx, |editor, cx| {
                editor
                    .listed_macros(cx)
                    .into_iter()
                    .map(|terminal_macro| terminal_macro.name)
                    .collect::<Vec<_>>()
            })
        };

        // Unfiltered, the list is folder then name.
        assert_eq!(
            listed(&editor, cx),
            vec!["メモ", "再起動", "ログイン", "設定保存"]
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.list_filter.update(cx, |filter, cx| {
                filter.set_text("大阪", window, cx);
            });
        });
        assert_eq!(listed(&editor, cx), vec!["再起動"]);

        editor.update_in(cx, |editor, window, cx| {
            editor.list_filter.update(cx, |filter, cx| {
                filter.set_text("定期", window, cx);
            });
        });
        assert_eq!(listed(&editor, cx), vec!["ログイン"]);
    }

    /// Every row of the editor's tree, folder headings marked by a trailing
    /// `/` and their macro count.
    fn tree_rows(editor: &Entity<MacroEditor>, cx: &mut VisualTestContext) -> Vec<String> {
        editor.update(cx, |editor, cx| {
            editor
                .macro_tree(cx)
                .into_iter()
                .map(|row| match row {
                    MacroTreeRow::Folder { folder, count, .. } => format!(
                        "{}/ ({count})",
                        folder.unwrap_or_else(|| UNFILED_FOLDER.to_string())
                    ),
                    MacroTreeRow::Macro(terminal_macro) => terminal_macro.name,
                })
                .collect::<Vec<_>>()
        })
    }

    fn stored_macro(
        editor: &Entity<MacroEditor>,
        name: &str,
        cx: &mut VisualTestContext,
    ) -> TerminalMacro {
        editor.update(cx, |editor, cx| {
            editor
                .store
                .read(cx)
                .macro_named(name)
                .unwrap_or_else(|| panic!("『{name}』 should be in the store"))
                .clone()
        })
    }

    #[gpui::test]
    async fn the_tree_groups_by_folder_and_collapses_a_heading(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "未分類/ (1)",
                "メモ",
                "拠点/大阪/ (1)",
                "再起動",
                "拠点/本社/ (2)",
                "ログイン",
                "設定保存",
            ]
        );

        // A collapsed heading keeps its count but hides what is under it.
        editor.update(cx, |editor, cx| {
            editor
                .collapsed_folders
                .insert(Some("拠点/本社".to_string()));
            cx.notify();
        });
        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "未分類/ (1)",
                "メモ",
                "拠点/大阪/ (1)",
                "再起動",
                "拠点/本社/ (2)",
            ]
        );
    }

    #[gpui::test]
    async fn the_tree_leaves_out_the_unfiled_heading_when_there_are_no_folders(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ログイン", "steps": [] }, { "name": "再起動", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        assert_eq!(tree_rows(&editor, cx), vec!["ログイン", "再起動"]);
    }

    #[gpui::test]
    async fn selecting_in_the_tree_loads_the_macro_into_the_form(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            assert!(
                !editor.has_unsaved_edits(cx),
                "a freshly opened blank form has nothing to discard"
            );
            editor.select_macro(Some(login), window, cx);
        });

        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_name.as_deref(), Some("ログイン"));
            assert_eq!(editor.name.read(cx).text(cx), "ログイン");
            assert_eq!(editor.folder.read(cx).text(cx), "拠点/本社");
            assert_eq!(editor.tags.read(cx).text(cx), "ルーター, 定期");
            assert!(editor.pending_selection.is_none());
            assert!(!editor.has_unsaved_edits(cx));
        });
    }

    #[gpui::test]
    async fn switching_away_from_unsaved_edits_asks_first(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, "ログイン", cx);
        let saved = stored_macro(&editor, "設定保存", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.add_step(StepKind::Clear, window, cx);
            assert!(editor.has_unsaved_edits(cx), "a new step is an edit");
            editor.select_macro(Some(saved.clone()), window, cx);
        });

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.original_name.as_deref(),
                Some("ログイン"),
                "the form must stay put until the edits are dealt with"
            );
            assert_eq!(
                editor
                    .pending_selection
                    .as_ref()
                    .and_then(|edited| edited.as_ref())
                    .map(|edited| edited.name.as_str()),
                Some("設定保存")
            );
            assert_eq!(editor.steps.len(), 1);
            assert!(editor.error.is_none());
        });

        // Backing out leaves the edits alone.
        editor.update(cx, |editor, cx| {
            editor.pending_selection = None;
            cx.notify();
        });
        editor.update_in(cx, |editor, window, cx| {
            assert_eq!(editor.steps.len(), 1);
            editor.select_macro(Some(saved), window, cx);
            editor.confirm_selection(window, cx);
        });
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_name.as_deref(), Some("設定保存"));
            assert_eq!(editor.name.read(cx).text(cx), "設定保存");
            assert!(editor.steps.is_empty(), "the discarded step must be gone");
            assert!(editor.pending_selection.is_none());
        });
    }

    #[gpui::test]
    async fn reselecting_the_open_macro_keeps_the_edits(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login.clone()), window, cx);
            editor.add_step(StepKind::Clear, window, cx);
            editor.select_macro(Some(login), window, cx);
        });
        editor.update(cx, |editor, _| {
            assert!(editor.pending_selection.is_none());
            assert_eq!(editor.steps.len(), 1);
        });
    }

    #[gpui::test]
    async fn renaming_a_folder_moves_every_macro_under_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let restart = stored_macro(&editor, "再起動", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(restart), window, cx);
            editor
                .collapsed_folders
                .insert(Some("拠点/大阪".to_string()));
            editor.begin_folder_rename("拠点".to_string(), window, cx);
            let Some((_, field)) = editor.renaming_folder.clone() else {
                panic!("the rename should be in progress");
            };
            field.update(cx, |field, cx| {
                field.set_text(" 全社 / ", window, cx);
            });
            editor.confirm_folder_rename(window, cx);
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        let folders = saved
            .macros
            .iter()
            .map(|terminal_macro| {
                (
                    terminal_macro.name.as_str(),
                    terminal_macro.folder.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            folders,
            vec![
                ("再起動", Some("全社/大阪")),
                ("設定保存", Some("全社/本社")),
                ("ログイン", Some("全社/本社")),
                ("メモ", None),
            ],
            "sub-folders travel with the folder that was renamed"
        );

        editor.update(cx, |editor, cx| {
            assert!(editor.renaming_folder.is_none());
            assert_eq!(
                editor.folder.read(cx).text(cx),
                "全社/大阪",
                "the open macro follows its folder"
            );
            assert!(
                !editor.has_unsaved_edits(cx),
                "a rename from the tree is not an unsaved form edit"
            );
            assert!(
                editor
                    .collapsed_folders
                    .contains(&Some("全社/大阪".to_string())),
                "the collapsed heading keeps its state under the new name"
            );
        });
    }

    #[gpui::test]
    async fn renaming_a_folder_to_nothing_is_refused(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.begin_folder_rename("拠点/本社".to_string(), window, cx);
            let Some((_, field)) = editor.renaming_folder.clone() else {
                panic!("the rename should be in progress");
            };
            field.update(cx, |field, cx| {
                field.set_text("  ", window, cx);
            });
            editor.confirm_folder_rename(window, cx);
        });
        editor.update(cx, |editor, _| {
            assert!(
                editor.renaming_folder.is_some(),
                "the field stays open so the name can be fixed"
            );
            assert_eq!(
                editor.error.as_deref(),
                Some("フォルダー名を入力してください。")
            );
        });
    }

    #[gpui::test]
    async fn moving_a_macro_files_it_under_another_folder(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let memo = stored_macro(&editor, "メモ", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(memo), window, cx);
            editor.move_macro_to_folder("メモ", Some("拠点/本社".to_string()), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "拠点/大阪/ (1)",
                "再起動",
                "拠点/本社/ (3)",
                "メモ",
                "ログイン",
                "設定保存",
            ]
        );
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.folder.read(cx).text(cx), "拠点/本社");
            assert!(!editor.has_unsaved_edits(cx));
        });

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved
                .macros
                .iter()
                .find(|terminal_macro| terminal_macro.name == "メモ")
                .and_then(|terminal_macro| terminal_macro.folder.as_deref()),
            Some("拠点/本社")
        );

        // Dropping it back on 未分類 clears the folder again.
        editor.update_in(cx, |editor, window, cx| {
            editor.move_macro_to_folder("メモ", None, window, cx);
        });
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.folder.read(cx).text(cx), "");
            assert_eq!(
                editor
                    .store
                    .read(cx)
                    .macro_named("メモ")
                    .and_then(|terminal_macro| terminal_macro.folder.as_deref()),
                None
            );
        });
    }

    #[gpui::test]
    async fn duplicating_opens_an_unused_copy_name(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "macros": [
                    {
                        "name": "ログイン",
                        "folder": "拠点/本社",
                        "steps": [{ "type": "send", "text": "enable" }]
                    },
                    { "name": "ログイン のコピー", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.duplicate_macro(window, cx);
        });

        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_name, None, "a copy is a new macro");
            assert_eq!(editor.name.read(cx).text(cx), "ログイン のコピー 2");
            assert_eq!(editor.folder.read(cx).text(cx), "拠点/本社");
            assert_eq!(editor.steps.len(), 1);
            assert!(
                editor.has_unsaved_edits(cx),
                "a copy is unsaved until 保存, so switching away must warn"
            );
        });

        editor.update(cx, |editor, cx| editor.save(cx));
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved
                .macros
                .iter()
                .map(|terminal_macro| terminal_macro.name.as_str())
                .collect::<Vec<_>>(),
            vec!["ログイン", "ログイン のコピー", "ログイン のコピー 2"]
        );
        assert_eq!(
            saved.macros[2].steps,
            vec![MacroStep::Send {
                text: "enable".to_string(),
                newline: true
            }],
            "the copy carries the steps it was made from"
        );
    }

    #[test]
    fn a_folder_rename_reaches_sub_folders_only() {
        assert_eq!(
            folder_after_rename("拠点", "拠点", "全社").as_deref(),
            Some("全社")
        );
        assert_eq!(
            folder_after_rename("拠点/本社", "拠点", "全社").as_deref(),
            Some("全社/本社")
        );
        // A folder that merely starts with the same characters is untouched.
        assert_eq!(folder_after_rename("拠点外", "拠点", "全社"), None);
        assert_eq!(folder_after_rename("別/拠点", "拠点", "全社"), None);
    }

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

    #[gpui::test]
    async fn spawning_into_the_terminal_panel_never_leases_the_workspace_twice(
        cx: &mut TestAppContext,
    ) {
        let (_fs, workspace, window) = init_workspace(cx, "{}").await;
        let cx = &mut VisualTestContext::from_window(*window, cx);

        workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel, window, cx);
        });

        let spawned = Rc::new(Cell::new(None));
        workspace.update_in(cx, {
            let spawned = spawned.clone();
            |_, window, cx| {
                cx.spawn_in(window, async move |workspace, cx| {
                    let task = SpawnInTerminal {
                        id: TaskId("terminal::launch-claude-code".to_string()),
                        full_label: "Claude Code".to_string(),
                        label: "Claude Code".to_string(),
                        command: Some(CLAUDE_CODE_PROGRAM.to_string()),
                        command_label: CLAUDE_CODE_PROGRAM.to_string(),
                        use_new_terminal: true,
                        allow_concurrent_runs: true,
                        reveal: RevealStrategy::Always,
                        reveal_target: RevealTarget::Dock,
                        ..SpawnInTerminal::default()
                    };
                    // `TerminalPanel::spawn_task` reads the workspace, so doing
                    // this from inside a `workspace.update_in` used to panic on
                    // a double lease before the terminal was ever created.
                    // Dropping the returned task cancels the pty work, which is
                    // not what is under test here.
                    spawned.set(Some(
                        spawn_task_in_terminal_panel(&workspace, &task, cx).is_some(),
                    ));
                })
                .detach();
            }
        });
        cx.run_until_parked();

        assert_eq!(
            spawned.get(),
            Some(true),
            "the spawn should reach the terminal panel"
        );
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
