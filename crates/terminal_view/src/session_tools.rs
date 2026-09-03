//! TeraTerm-style session tooling: quick connect, the macro picker and the
//! macro editor.
//!
//! The data these modals read and write lives in `terminal_sessions.json`
//! (see [`terminal::terminal_macro`]); this module is only the UI over it.

use std::sync::Arc;

use editor::Editor;
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
    terminal_macro::{
        ConnectionProtocol, MacroStep, SavedConnection, TerminalMacro, TerminalSessionStore,
    },
    terminal_settings::TerminalSettings,
};
use ui::{
    Checkbox, ContextMenu, DropdownMenu, DropdownStyle, ListItem, ListItemSpacing, Modal,
    ModalFooter, ModalHeader, Section, SectionHeader, ToggleState, Tooltip, prelude::*,
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
        let macros = store.read(cx).macros().to_vec();
        let delegate = MacroPickerDelegate {
            store: store.clone(),
            workspace,
            matches: (0..macros.len()).collect(),
            macros,
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

/// Shown when a save is attempted before `terminal_sessions.json` has been read
/// once; writing then would rewrite the file from an empty in-memory copy.
const LOADING_ERROR: &str =
    "設定ファイルの読み込みが終わっていません。少し待ってからもう一度お試しください。";

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
    /// Name the macro had when the form was last loaded, so a rename replaces
    /// the original entry instead of adding a second one.
    original_name: Option<String>,
    name: Entity<Editor>,
    description: Entity<Editor>,
    steps: Vec<StepRow>,
    error: Option<SharedString>,
    /// Macro whose 削除 has been asked for but not yet confirmed.
    pending_delete: Option<String>,
    focus_handle: FocusHandle,
    _store_loaded: Subscription,
}

impl MacroEditor {
    pub fn new(
        store: Entity<TerminalSessionStore>,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("マクロ名", window, cx);
            editor
        });
        let description = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("説明 (省略可)", window, cx);
            editor
        });
        let focus_handle = cx.focus_handle();
        // `terminal_sessions.json` is read asynchronously, so an editor opened
        // before the first read landed would otherwise show an empty list of
        // saved macros for as long as it stays open.
        let store_loaded = cx.observe(&store, |_, _, cx| cx.notify());

        let mut this = Self {
            store,
            original_name: None,
            name,
            description,
            steps: Vec::new(),
            error: None,
            pending_delete: None,
            focus_handle,
            _store_loaded: store_loaded,
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
        self.description.update(cx, |editor, cx| {
            editor.set_text(edited.description, window, cx);
        });
        self.steps = edited
            .steps
            .iter()
            .map(|step| StepRow::new(step, window, cx))
            .collect();
        self.error = None;
        self.pending_delete = None;
        window.focus(&self.name.focus_handle(cx), cx);
        cx.notify();
    }

    /// The saved macros the list shows. Read straight from the store so the
    /// list is whatever is on disk right now, not a copy taken at open time.
    fn listed_macros(&self, cx: &App) -> Vec<TerminalMacro> {
        self.store.read(cx).macros().to_vec()
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

        cx.spawn(async move |this, cx| {
            if let Err(error) = write.await {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("削除できませんでした: {error}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn render_macro_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.store.read(cx).is_loaded() {
            return Label::new("読み込み中…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }

        let macros = self.listed_macros(cx);
        if macros.is_empty() {
            return Label::new("保存済みのマクロがありません")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }

        let editing = self.original_name.clone();
        v_flex()
            .id("saved-macros")
            .max_h(rems(10.))
            .overflow_y_scroll()
            .gap_0p5()
            .children(
                macros
                    .into_iter()
                    .enumerate()
                    .map(|(index, terminal_macro)| {
                        let name = terminal_macro.name.clone();
                        let selected = editing.as_deref() == Some(name.as_str());
                        let detail = format!("{} ステップ", terminal_macro.steps.len());
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_between()
                            .child(
                                Button::new(("saved-macro", index), name.clone())
                                    .full_width()
                                    .toggle_state(selected)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.load_macro(Some(terminal_macro.clone()), window, cx);
                                    })),
                            )
                            .child(
                                Label::new(detail)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                IconButton::new(("delete-saved-macro", index), IconName::Trash)
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("このマクロを削除します"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.request_delete(name.clone(), cx);
                                    })),
                            )
                    }),
            )
            .into_any_element()
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
        let saved_macros = self.render_macro_list(cx);
        let form_header = match &self.original_name {
            Some(name) => format!("『{name}』を編集"),
            None => "新規マクロ".to_string(),
        };
        let editing = self.original_name.clone();
        let pending_delete = self.pending_delete.clone();

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
                        Section::new()
                            .header(
                                SectionHeader::new("保存済みのマクロ").end_slot(
                                    Button::new("new-macro", "新規作成")
                                        .start_icon(Icon::new(IconName::Plus))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.load_macro(None, window, cx);
                                        })),
                                ),
                            )
                            .child(saved_macros),
                    )
                    .section(
                        Section::new()
                            .header(SectionHeader::new(form_header))
                            .child(
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
                                        this.child(Label::new(error).color(Color::Error))
                                    }),
                            ),
                    )
                    .footer(
                        ModalFooter::new()
                            .when_some(editing, |footer, name| {
                                footer.start_slot(
                                    Button::new("delete-macro", "削除")
                                        .color(Color::Error)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.request_delete(name.clone(), cx);
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
    host: Entity<Editor>,
    port: Entity<Editor>,
    user: Entity<Editor>,
    protocol: ConnectionProtocol,
    macro_name: Option<String>,
    log: bool,
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
        // The saved-connection list and the macro dropdown both read the store
        // on every render, and the file lands asynchronously.
        let store_loaded = cx.observe(&store, |_, _, cx| cx.notify());

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
            _store_loaded: store_loaded,
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
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
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
        if !self.store.read(cx).is_loaded() {
            return Label::new("読み込み中…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }
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

    fn picker_names(picker: &Entity<MacroPicker>, cx: &mut VisualTestContext) -> Vec<String> {
        picker.update(cx, |picker, cx| {
            let picker = picker.picker.read(cx);
            (0..picker.delegate.match_count())
                .filter_map(|index| {
                    let index = *picker.delegate.matches.get(index)?;
                    Some(picker.delegate.macros.get(index)?.name.clone())
                })
                .collect()
        })
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
            assert_eq!(names, vec!["ログイン", "設定保存", "再起動"]);
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
            vec!["既存".to_string(), "新しいマクロ".to_string()]
        );
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
