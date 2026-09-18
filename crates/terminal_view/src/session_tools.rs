//! TeraTerm-style session tooling: quick connect, the macro picker and the
//! macro editor.
//!
//! The data these modals read and write lives in `terminal_sessions.json`
//! (see [`terminal::terminal_macro`]); this module is only the UI over it.

use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use collections::{HashMap, HashSet};
use editor::{Editor, EditorEvent};
use futures::StreamExt as _;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, AsyncWindowContext, BackgroundExecutor, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, FutureExt as _, Render, Subscription, Task, WeakEntity, Window, actions,
};
use picker::{Picker, PickerDelegate};
use project::Fs;
use remote::{CommandTemplate, Interactive};
use settings::Settings as _;
use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
use terminal::{
    Terminal,
    quick_connect_credentials::{self, QuickConnectCredentials, StoredPassword},
    terminal_macro::{
        ConnectionProtocol, MacroFolder, MacroKey, MacroStep, MacroStepList, MatchScope,
        SavedConnection, TerminalMacro, TerminalSessionStore, TerminalSessionsFile,
        normalize_folder, normalize_tags,
    },
    terminal_settings::TerminalSettings,
};
use ui::{
    Checkbox, ContextMenu, Disclosure, DropdownMenu, DropdownStyle, HighlightedLabel,
    InputFrameSize, ListItem, ListItemSpacing, Modal, ModalFooter, ModalHeader, Section,
    ToggleState, Tooltip, input_frame, input_frame_sized, prelude::*, right_click_menu,
};
use util::ResultExt as _;
use util::command::Stdio;
use workspace::{ModalView, Panel as _, Workspace};
use zed_actions::terminal::InstallClaudeCode;

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
    })
    .detach();
}

/// Starts the `claude` on this machine's `PATH` in a new terminal tab, outside
/// any sandbox (ADR-F49 decision 6). The caller asks the user first, every time:
/// `agent_ui` owns the launch actions because the sandboxed launch needs the IDE
/// session and the Linux CLI, which this crate cannot reach.
pub fn spawn_claude_code_on_host(
    workspace: &mut Workspace,
    effort: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    spawn_claude_code_command(workspace, ClaudeCodeCommand::Launch { effort }, window, cx);
}

/// 公式パッケージ名。フォークは Claude Code を同梱せず (ADR-F20)、公式の npm
/// インストールコマンドをそのまま実行するだけにする。Zed 同梱の Node ランタイムは
/// 使わず、ユーザーの PATH 上の `npm` を使う。
const CLAUDE_CODE_PACKAGE: &str = "@anthropic-ai/claude-code";
const NPM_PROGRAM: &str = "npm";
const CLAUDE_CODE_PROGRAM: &str = "claude";

/// 接続先 (リモートプロジェクト) では、`SpawnInTerminal` の program をそのまま渡すと
/// トランスポートが `cd … && exec env <program>` を組み立てる (ログインシェルを通らない) ため、
/// 公式インストーラーの置き場 `~/.local/bin` が PATH に無く「見つかりません」になる。
/// ログインシェルを通し、そこに `~/.local/bin` を足したうえで起動する。
const REMOTE_SHELL: &str = "/bin/sh";
/// `$0`。スクリプトの引数は `$1` から始まる。
const REMOTE_SCRIPT_NAME: &str = "zed-claude-code";
/// `$@` をそのまま `exec` するので、プログラム名も引数もクォートしないで渡せる。
/// 改行ではなく `;` で区切るのは、この本文を 1 引数に引用するのが接続先のログインシェルで、
/// csh 系は引用の中の改行を受け付けないため。
const REMOTE_LAUNCH_SCRIPT: &str = "PATH=\"$HOME/.local/bin:$PATH\"; export PATH; exec \"$@\"";
/// 起動と同じ PATH で `$1` を探すだけ。見つからなければ終了コードが 0 以外になる。
const REMOTE_LOOKUP_SCRIPT: &str =
    "PATH=\"$HOME/.local/bin:$PATH\"; export PATH; command -v -- \"$1\" >/dev/null 2>&1";

/// `script` を接続先のログインシェルで実行し、`program args…` をその位置引数として渡す形。
fn remote_invocation(script: &str, program: &str, args: &[String]) -> (String, Vec<String>) {
    let mut wrapped = vec![
        "-l".to_string(),
        "-c".to_string(),
        script.to_string(),
        REMOTE_SCRIPT_NAME.to_string(),
        program.to_string(),
    ];
    wrapped.extend(args.iter().cloned());
    (REMOTE_SHELL.to_string(), wrapped)
}

enum ClaudeCodeCommand {
    Install,
    Launch { effort: Option<String> },
}

fn claude_code_program(command: &ClaudeCodeCommand) -> &'static str {
    match command {
        ClaudeCodeCommand::Install => NPM_PROGRAM,
        ClaudeCodeCommand::Launch { .. } => CLAUDE_CODE_PROGRAM,
    }
}

/// 起動前にプログラムの在処を確かめる手立て。接続先では `command -v` を 1 往復で走らせる
/// (proto は増やさない)。確かめようが無ければ `None` で、そのまま起動して端末に結果を出す。
enum ProgramLookup {
    OnThisMachine(String),
    OnRemoteHost(CommandTemplate),
    /// テスト用: 返事を返さない接続先。
    #[cfg(test)]
    NeverAnswers,
}

/// `command -v` の 1 往復を待つ時間 (ADR-F53)。起動前の確認でしかないので、
/// 繋がったまま黙り込んだ接続先のためにボタンを押した人を待たせない。
const REMOTE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Windows の OpenSSH には `ControlMaster` が無い (`crates/remote` の
/// `SshSocket::ssh_command_options` が非 Windows でしかソケットを渡さない) ので、
/// この 1 往復は毎回 TCP の接続と認証から始まる。既存の接続に相乗りできる他の OS
/// より長く待たないと、遅い回線で毎回「確かめられなかった」に倒れる。
const REMOTE_LOOKUP_TIMEOUT_WITHOUT_CONTROL_MASTER: Duration = Duration::from_secs(30);

fn remote_lookup_timeout() -> Duration {
    if cfg!(windows) {
        REMOTE_LOOKUP_TIMEOUT_WITHOUT_CONTROL_MASTER
    } else {
        REMOTE_LOOKUP_TIMEOUT
    }
}

/// 起動前の在処確認ひとそろい。どのタブに開く経路でも同じものを通すので、接続先の
/// プロジェクトでも事前確認と案内文が飛ばされない (ADR-F53)。
pub struct ClaudeCodeLookup {
    /// 確かめようが無ければ `None`。そのまま起動して端末に結果を出す。
    lookup: Option<ProgramLookup>,
    program: &'static str,
    on_remote_host: bool,
}

/// 公式 CLI を起動するタスク。**どこのタブに開くかは呼び出し元が決める**:
/// 起動の入り口は `agent_ui` にあり (サンドボックス経路が IDE セッションを使うため)、
/// エージェントパネルのタブに開く経路 (ADR-F53) は組み立てだけをここから使う。
pub fn claude_code_launch_task(
    effort: Option<String>,
    workspace: &Workspace,
    cx: &App,
) -> SpawnInTerminal {
    claude_code_task(&ClaudeCodeCommand::Launch { effort }, workspace, cx)
}

/// 起動前に `claude` の在処を確かめる手立て。[`claude_code_launch_task`] と対で使う。
pub fn claude_code_launch_lookup(workspace: &Workspace, cx: &App) -> ClaudeCodeLookup {
    claude_code_lookup(CLAUDE_CODE_PROGRAM, workspace, cx)
}

fn claude_code_lookup(program: &'static str, workspace: &Workspace, cx: &App) -> ClaudeCodeLookup {
    let remote_client = workspace.project().read(cx).remote_client();
    let lookup = match &remote_client {
        Some(remote_client) => {
            let (lookup_program, lookup_args) =
                remote_invocation(REMOTE_LOOKUP_SCRIPT, program, &[]);
            remote_client
                .read(cx)
                .build_command(
                    Some(lookup_program),
                    &lookup_args,
                    &HashMap::default(),
                    None,
                    None,
                    Interactive::No,
                )
                .map(ProgramLookup::OnRemoteHost)
                // 接続が落ちている等で組み立てられないなら確かめずに起動し、
                // 端末に本当の失敗理由を出させる。
                .log_err()
        }
        None => Some(ProgramLookup::OnThisMachine(program.to_string())),
    };
    ClaudeCodeLookup {
        lookup,
        program,
        on_remote_host: remote_client.is_some(),
    }
}

impl ClaudeCodeLookup {
    /// 起動を止めるなら案内文、起動してよければ `None`。
    pub async fn missing_message(self, executor: &BackgroundExecutor) -> Option<String> {
        let lookup = self.lookup?;
        let found = executor
            .spawn(async move { lookup.find().await })
            .with_timeout(remote_lookup_timeout(), executor)
            .await
            // 期限までに答えが返らないのは「無い」ではないので、起動へ進んで
            // 本当の失敗理由を端末に出させる (`find` が答えられないときと同じ)。
            .unwrap_or_else(|_| {
                log::warn!(
                    "the host did not answer whether it has the CLI within {} seconds",
                    remote_lookup_timeout().as_secs()
                );
                true
            });
        (!found).then(|| claude_code_missing_program_message(self.program, self.on_remote_host))
    }
}

/// 公式 CLI / 公式インストールコマンドを実行するタスクを組み立てる。
///
/// ここで組み立てるコマンドは**人間がターミナルに打つのと同じ実行経路**なので、
/// ADR-F18 (リモートのエージェント実行と違い人間のターミナルは対象外) /
/// ADR-F19 と同じ扱いで **egress 許可リストの対象外**。プロキシ注入も
/// 許可リストへのホスト追加も不要で、ここでは一切行わない。
fn claude_code_task(
    command: &ClaudeCodeCommand,
    workspace: &Workspace,
    cx: &App,
) -> SpawnInTerminal {
    let project = workspace.project().read(cx);
    claude_code_spawn(
        command,
        crate::first_project_directory(workspace, cx),
        project.remote_client().is_some(),
    )
}

/// [`claude_code_task`] の中身。どこのタブに開く経路も同じものを通すので、
/// 接続先の包み ([`remote_invocation`]) がどれか一方だけに掛かることがない
/// (ADR-F53)。
fn claude_code_spawn(
    command: &ClaudeCodeCommand,
    cwd: Option<PathBuf>,
    on_remote_host: bool,
) -> SpawnInTerminal {
    let (program, args, task_id, label) = match command {
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
                "Claude Code (サンドボックスの外)".to_string(),
            )
        }
    };

    // 包む前の論理的なコマンド行。端末に見せるのはこちらで、接続先用の `/bin/sh -l -c …` は
    // 見せない。
    let command_label = if args.is_empty() {
        program.clone()
    } else {
        format!("{program} {}", args.join(" "))
    };
    // 接続先ではログインシェルを通す ([`remote_invocation`])。
    let (program, args) = if on_remote_host {
        remote_invocation(REMOTE_LAUNCH_SCRIPT, &program, &args)
    } else {
        (program, args)
    };

    SpawnInTerminal {
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
        // Rerunning would open the task picker: these commands are not tasks.
        show_rerun: false,
        ..SpawnInTerminal::default()
    }
}

/// 組み立てたタスクを統合ターミナルの新しいタブで実行する。
fn spawn_claude_code_command(
    workspace: &mut Workspace,
    command: ClaudeCodeCommand,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let lookup = claude_code_lookup(claude_code_program(&command), workspace, cx);
    let spawn_task = claude_code_task(&command, workspace, cx);

    cx.spawn_in(window, async move |workspace, cx| {
        {
            let executor = cx.background_executor().clone();
            if let Some(message) = lookup.missing_message(&executor).await {
                workspace
                    .update(cx, |workspace, cx| workspace.show_error(message, cx))
                    .ok();
                return;
            }
        }

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

impl ProgramLookup {
    /// そのプログラムを起動できるか。確かめられなかったときは「ある」として扱い、
    /// 本当の失敗理由は端末に出させる。
    async fn find(self) -> bool {
        match self {
            #[cfg(test)]
            Self::NeverAnswers => std::future::pending().await,
            Self::OnThisMachine(program) => which::which(&program).is_ok(),
            Self::OnRemoteHost(template) => {
                let mut command = util::command::new_command(&template.program);
                command
                    .args(&template.args)
                    .envs(template.env)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true);
                match command.output().await {
                    // `command -v` の答えだけを答えとして扱う (見つからない =
                    // sh は 127、bash は 1)。ssh 自身が失敗したとき (255 など) に
                    // 「接続先に claude がありません」と言うのは嘘になるので、
                    // 確かめられなかった扱いにして端末に本当の理由を出させる。
                    Ok(output) => match output.status.code() {
                        Some(0) => true,
                        Some(1) | Some(127) => false,
                        code => {
                            log::warn!(
                                "the host did not answer whether it has the CLI (exit {code:?}): {}",
                                String::from_utf8_lossy(&output.stderr).trim()
                            );
                            true
                        }
                    },
                    Err(error) => {
                        log::warn!("could not ask the host whether it has the CLI: {error:#}");
                        true
                    }
                }
            }
        }
    }
}

fn claude_code_missing_program_message(program: &str, on_remote_host: bool) -> String {
    match (program, on_remote_host) {
        (NPM_PROGRAM, false) => {
            "npm が PATH 上に見つかりません。Claude Code のインストールには Node.js が必要です。\
             https://nodejs.org/ から Node.js をインストールしてから、もう一度お試しください。"
                .to_string()
        }
        (NPM_PROGRAM, true) => "接続先のホストに npm が見つかりません。接続先のターミナルで \
             Node.js をインストールしてから、もう一度お試しください。"
            .to_string(),
        (_, false) => "claude が PATH 上に見つかりません。先に「Claude Code をインストール \
             (公式コマンド)」を実行してください。"
            .to_string(),
        // ADR-F49 決定 6: 接続先へこちらから勝手にインストールはしない。公式手順を案内する。
        (_, true) => "接続先のホストに claude が見つかりません。接続先のターミナルで次の\
             コマンドを実行してインストールしてください。\n\
             curl -fsSL https://claude.ai/install.sh | bash\n\
             インストールすると `~/.local/bin/claude` に入り、この起動から使えるようになります。"
            .to_string(),
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
            session_collapsed: HashMap::default(),
            folders_collapsed_by_default: TerminalSettings::get_global(cx)
                .macro_picker_folders_collapsed,
            selected_index: 0,
        };
        // Not `uniform_list`: that lays every row out at one height, and a
        // folder heading is not built like a macro row (no `ListItem` inset,
        // its own padding), so the headings after the first were drawn on top
        // of the row above them.
        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                // Long macro lists have to scroll rather than grow the modal
                // past the screen; roughly a dozen rows before the scrollbar
                // takes over.
                .max_height(rems(20.))
                .show_scrollbar(true)
                // A `gpui::list` only knows the height of the rows it has
                // drawn, so without this both the scrollbar thumb and jumping
                // to a row far down the list (End, or a query that selects one)
                // work off zero-height rows. Measuring up front is affordable
                // here: the rows are one line of text each.
                .list_measure_all()
        });
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
    Folder {
        /// `None` is the [`UNFILED_FOLDER`] heading, which has no folder of its
        /// own in the file and so no stored collapse state.
        folder: Option<String>,
        /// How many macros the heading stands for, matches only.
        count: usize,
        collapsed: bool,
    },
    Macro {
        index: usize,
        /// Byte offsets of the matched characters within the macro's *name*,
        /// which is the only part of the search text the row shows.
        name_highlights: Vec<usize>,
    },
}

impl MacroPickerEntry {
    /// 「ルータ用 (2)」, the heading the picker draws.
    fn heading_label(folder: Option<&str>, count: usize) -> String {
        format!("{} ({count})", folder.unwrap_or(UNFILED_FOLDER))
    }
}

/// Folds a folder heading open or shut *in the file*, so the editor tree and
/// the runner picker agree and the state outlives the modal. `None` when there
/// is nothing to write, which the caller treats as success.
fn write_folder_collapsed(
    store: &Entity<TerminalSessionStore>,
    folder: &str,
    collapsed: bool,
    cx: &mut App,
) -> Option<Task<anyhow::Result<()>>> {
    // Writing before the first read would rewrite the file from an empty
    // in-memory copy, dropping every macro and connection already in it.
    if !store.read(cx).is_loaded() {
        return None;
    }
    let mut file = store.read(cx).file().clone();
    let entry = file
        .macro_folders
        .iter_mut()
        .find(|candidate| candidate.name == folder)?;
    if entry.collapsed == collapsed {
        return None;
    }
    entry.collapsed = collapsed;
    Some(store.update(cx, |store, cx| store.write(file, cx)))
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

/// Macros are always listed in folder order — the order the user arranged
/// `macro_folders` in, 未分類 last — and by name inside a folder, whether or not
/// a query narrowed them, so the same macro is always found in the same place.
fn sort_macros(macros: &mut [TerminalMacro], folders: &[MacroFolder]) {
    let rank = |folder: Option<&str>| {
        folder
            .and_then(|folder| {
                folders
                    .iter()
                    .position(|candidate| candidate.name == folder)
            })
            .unwrap_or(usize::MAX)
    };
    macros.sort_by(|left, right| {
        rank(left.folder.as_deref())
            .cmp(&rank(right.folder.as_deref()))
            // Only reached by a macro whose folder is not registered, which
            // `ensure_macro_folders` normally repairs on load.
            .then_with(|| left.folder_display().cmp(right.folder_display()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

pub struct MacroPickerDelegate {
    store: Entity<TerminalSessionStore>,
    workspace: WeakEntity<Workspace>,
    /// Sorted in folder order then by name; [`MacroPickerDelegate::entries`]
    /// indexes it.
    macros: Vec<TerminalMacro>,
    entries: Vec<MacroPickerEntry>,
    /// Headings the user folded or unfolded *in this picker*, keyed by folder
    /// (`None` is 未分類, which has no folder in the file to keep it in). It
    /// overrides both the stored per-folder state and
    /// [`Self::folders_collapsed_by_default`], and is thrown away with the modal.
    session_collapsed: HashMap<Option<String>, bool>,
    /// `terminal.macro_picker_folders_collapsed`, read once when the picker
    /// opens so that changing the setting cannot reshuffle a list the user is
    /// looking at.
    folders_collapsed_by_default: bool,
    selected_index: usize,
}

impl MacroPickerDelegate {
    fn selected_macro(&self) -> Option<&TerminalMacro> {
        match self.entries.get(self.selected_index)? {
            MacroPickerEntry::Macro { index, .. } => self.macros.get(*index),
            MacroPickerEntry::Folder { .. } => None,
        }
    }

    fn first_selectable_index(&self) -> usize {
        self.entries
            .iter()
            .position(|entry| matches!(entry, MacroPickerEntry::Macro { .. }))
            .unwrap_or(0)
    }

    /// Whether `folder`'s heading is folded shut: what this picker was told
    /// during this session, else the setting, else what the file remembers.
    /// 未分類 has no entry in the file, so it is open unless something above
    /// says otherwise.
    fn is_collapsed(&self, folder: Option<&str>, folders: &[MacroFolder]) -> bool {
        if let Some(collapsed) = self.session_collapsed.get(&folder.map(str::to_string)) {
            return *collapsed;
        }
        if self.folders_collapsed_by_default {
            return true;
        }
        folder.is_some_and(|folder| {
            folders
                .iter()
                .any(|candidate| candidate.name == folder && candidate.collapsed)
        })
    }

    /// Turns the matches into rows, one heading per folder in the order the
    /// user arranged them, 未分類 last. `honor_collapse` is false while a query
    /// narrows the list: a match must never be hidden behind a folded heading.
    fn build_entries(
        &self,
        macros: &[TerminalMacro],
        folders: &[MacroFolder],
        mut matches: Vec<StringMatch>,
        honor_collapse: bool,
    ) -> Vec<MacroPickerEntry> {
        // `match_strings` returns best-scoring first; the list is ordered by
        // folder and name instead, and `candidate_id` is that position.
        matches.sort_by_key(|string_match| string_match.candidate_id);

        let mut entries = Vec::with_capacity(matches.len());
        let mut start = 0;
        while start < matches.len() {
            let Some(first) = macros.get(matches[start].candidate_id) else {
                start += 1;
                continue;
            };
            let folder = first.folder.clone();
            let mut end = start + 1;
            while end < matches.len()
                && macros
                    .get(matches[end].candidate_id)
                    .is_some_and(|candidate| candidate.folder == folder)
            {
                end += 1;
            }

            // With no folders at all the list is flat, the way it was before
            // folders became something the user arranges.
            let heading = folder.is_some() || !folders.is_empty();
            let collapsed = honor_collapse && self.is_collapsed(folder.as_deref(), folders);
            if heading {
                entries.push(MacroPickerEntry::Folder {
                    folder: folder.clone(),
                    count: end - start,
                    collapsed,
                });
            }
            if !(heading && collapsed) {
                for string_match in &matches[start..end] {
                    let Some(terminal_macro) = macros.get(string_match.candidate_id) else {
                        continue;
                    };
                    entries.push(MacroPickerEntry::Macro {
                        index: string_match.candidate_id,
                        name_highlights: name_highlights(terminal_macro, &string_match.positions),
                    });
                }
            }
            start = end;
        }
        entries
    }

    /// Folds the heading the user clicked. The decision always holds for this
    /// picker; it is also written to the file (so the editor tree and the next
    /// session agree) unless the setting is folding everything anyway, in which
    /// case there is nothing for the file to say.
    fn toggle_folder(
        picker: &mut Picker<Self>,
        folder: Option<String>,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        picker
            .delegate
            .session_collapsed
            .insert(folder.clone(), !collapsed);
        let persist = !picker.delegate.folders_collapsed_by_default;
        picker.refresh(window, cx);
        // 未分類 has no folder in the file to keep the state in.
        let Some(folder) = folder.filter(|_| persist) else {
            return;
        };
        let store = picker.delegate.store.clone();
        if let Some(write) = write_folder_collapsed(&store, &folder, !collapsed, cx) {
            cx.spawn(async move |_, _| {
                write.await.log_err();
            })
            .detach();
        }
    }

    /// Whether every heading the list is showing is folded shut, which is what
    /// the 「すべて展開 / すべて折りたたむ」 button flips.
    fn every_folder_collapsed(&self) -> bool {
        let mut headings = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MacroPickerEntry::Folder { collapsed, .. } => Some(*collapsed),
                MacroPickerEntry::Macro { .. } => None,
            })
            .peekable();
        headings.peek().is_some() && headings.all(|collapsed| collapsed)
    }

    /// Folds or unfolds every heading at once. Session-only: this is a way of
    /// looking through the list, not a change to how the folders are filed, so
    /// nothing is written to `terminal_sessions.json`.
    fn set_every_folder_collapsed(
        picker: &mut Picker<Self>,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let folders = picker
            .delegate
            .store
            .read(cx)
            .macro_folders()
            .iter()
            .map(|folder| Some(folder.name.clone()))
            .chain([None])
            .collect::<Vec<_>>();
        for folder in folders {
            picker.delegate.session_collapsed.insert(folder, collapsed);
        }
        picker.refresh(window, cx);
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
                matches!(entry, MacroPickerEntry::Folder { .. })
                    .then(|| index.checked_sub(1))
                    .flatten()
            })
            .collect()
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "フォルダー / 名前 / タグで絞り込み…".into()
    }

    /// 「すべて展開 / すべて折りたたむ」. Sits on the query row so it is always
    /// reachable, and only appears once there is a heading to fold.
    fn searchbar_trailer(
        &self,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        if !self
            .entries
            .iter()
            .any(|entry| matches!(entry, MacroPickerEntry::Folder { .. }))
        {
            return None;
        }
        let collapsed = self.every_folder_collapsed();
        let (icon, label) = if collapsed {
            (IconName::ChevronUpDown, "すべて展開")
        } else {
            (IconName::ChevronDownUp, "すべて折りたたむ")
        };
        Some(
            IconButton::new("macro-picker-toggle-all-folders", icon)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(label))
                .on_click(cx.listener(move |picker, _, window, cx| {
                    Self::set_every_folder_collapsed(picker, !collapsed, window, cx);
                }))
                .into_any_element(),
        )
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
        let folders = self.store.read(cx).macro_folders().to_vec();
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros, &folders);
        let candidates = macros
            .iter()
            .enumerate()
            .map(|(index, terminal_macro)| {
                StringMatchCandidate::new(index, &macro_search_text(terminal_macro))
            })
            .collect::<Vec<_>>();
        let query = query.trim().to_string();
        // A folded heading must never swallow a match, so the collapse state is
        // only honoured while the list stands for everything the user has.
        let honor_collapse = query.is_empty();
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
                    delegate.entries =
                        delegate.build_entries(&macros, &folders, matches, honor_collapse);
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
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.entries.get(ix)? {
            MacroPickerEntry::Folder {
                folder,
                count,
                collapsed,
            } => {
                let label = MacroPickerEntry::heading_label(folder.as_deref(), *count);
                let folder = folder.clone();
                let collapsed = *collapsed;
                let disclosure_folder = folder.clone();
                Some(
                    h_flex()
                        .w_full()
                        .px_2()
                        .pt_1()
                        .gap_1()
                        .debug_selector(|| picker_row_selector(ix))
                        .child(
                            Disclosure::new(("macro-picker-folder-toggle", ix), !collapsed)
                                .on_click(cx.listener(move |picker, _, window, cx| {
                                    Self::toggle_folder(
                                        picker,
                                        disclosure_folder.clone(),
                                        collapsed,
                                        window,
                                        cx,
                                    );
                                })),
                        )
                        .child(
                            h_flex()
                                .id(("macro-picker-folder", ix))
                                .gap_1()
                                .cursor_pointer()
                                .on_click(cx.listener(move |picker, _, window, cx| {
                                    Self::toggle_folder(
                                        picker,
                                        folder.clone(),
                                        collapsed,
                                        window,
                                        cx,
                                    );
                                }))
                                .child(
                                    Icon::new(IconName::Folder)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(label).size(LabelSize::Small).color(Color::Muted),
                                ),
                        )
                        .into_any_element(),
                )
            }
            MacroPickerEntry::Macro {
                index,
                name_highlights,
            } => {
                let terminal_macro = self.macros.get(*index)?;
                let description = (!terminal_macro.description.is_empty())
                    .then(|| terminal_macro.description.clone());
                Some(
                    ListItem::new(ix)
                        .inset(true)
                        .spacing(ListItemSpacing::Sparse)
                        .toggle_state(selected)
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap_2()
                                .debug_selector(|| picker_row_selector(ix))
                                .child(HighlightedLabel::new(
                                    terminal_macro.name.clone(),
                                    name_highlights.clone(),
                                ))
                                // The row is one line, so anything that does not
                                // fit is cut rather than wrapped; the tags are
                                // left out of it entirely and stay searchable
                                // through `macro_search_text`.
                                .when_some(description, |this, description| {
                                    this.child(
                                        Label::new(description)
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .truncate(),
                                    )
                                }),
                        )
                        .end_slot(
                            div().debug_selector(|| picker_detail_selector(ix)).child(
                                Label::new(macro_row_detail(terminal_macro))
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

/// The muted right-hand side of a picker row: how many steps the macro has, and
/// how often it repeats when that is not once.
fn macro_row_detail(terminal_macro: &TerminalMacro) -> String {
    let steps = format!("{} ステップ", terminal_macro.steps.len());
    match terminal_macro.iteration_count() {
        Some(1) => steps,
        Some(count) => format!("{steps} / {count} 回"),
        None => format!("{steps} / 中止まで"),
    }
}

/// Names a picker row for [`gpui::VisualTestContext::debug_bounds`], so a test
/// can check that the rows — one-line folder headings and one-line macro rows,
/// which are still built differently — are laid out without landing on top of
/// each other. Compiles away outside test builds.
fn picker_row_selector(ix: usize) -> String {
    format!("macro-picker-row-{ix}")
}

/// Names the muted 「n ステップ」 slot of a picker row, so a test can check it
/// really sits to the right of the name rather than on a second line.
fn picker_detail_selector(ix: usize) -> String {
    format!("macro-picker-detail-{ix}")
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepKind {
    Send,
    WaitFor,
    Sleep,
    Clear,
    Repeat,
    If,
}

impl StepKind {
    fn label(&self) -> &'static str {
        match self {
            StepKind::Send => "送信",
            StepKind::WaitFor => "期待文字を待つ",
            StepKind::Sleep => "秒待ち",
            StepKind::Clear => "画面を消去",
            StepKind::Repeat => "繰り返し",
            StepKind::If => "分岐",
        }
    }

    /// Whether the row shows the text field (期待文字 / 送信する文字列 /
    /// 判定する文字列).
    fn has_text(&self) -> bool {
        matches!(self, StepKind::Send | StepKind::WaitFor | StepKind::If)
    }

    /// Whether the row shows the seconds field.
    fn has_duration(&self) -> bool {
        matches!(self, StepKind::WaitFor | StepKind::Sleep)
    }

    /// Whether the row matches against the output, and so offers 正規表現.
    fn has_pattern(&self) -> bool {
        matches!(self, StepKind::WaitFor | StepKind::If)
    }

    /// The grey hint of the row's text field. It names the field and says that
    /// captured values may be spliced in, which is the only place the syntax is
    /// discoverable from the form.
    fn text_placeholder(&self) -> &'static str {
        match self {
            StepKind::Send => "送信する文字列 (${変数} が使えます)",
            StepKind::WaitFor => "期待文字 (${変数} が使えます)",
            StepKind::If => "判定する文字列 (${変数} が使えます)",
            StepKind::Sleep | StepKind::Clear | StepKind::Repeat => "",
        }
    }

    const ALL: [StepKind; 6] = [
        StepKind::Send,
        StepKind::WaitFor,
        StepKind::Sleep,
        StepKind::Clear,
        StepKind::Repeat,
        StepKind::If,
    ];
}

/// Which of a row's child lists a path descends into.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StepList {
    /// The macro's own steps, the body of a 繰り返し, or the 「そのとき」 side of
    /// a 分岐.
    Main,
    /// The 「それ以外」 side of a 分岐.
    Else,
    /// The 「タイムアウト時」 steps of a 期待文字を待つ.
    Timeout,
}

impl StepList {
    /// The token [`StepPath::label`] and [`StepPath::debug_selector`] put in
    /// front of a level's 1-based index. Only `Else` and `Timeout` carry one,
    /// which is what keeps a step of 「そのとき」 and the step at the same
    /// position in 「それ以外」 from reading as the same step number.
    ///
    /// Delegated to the runner's own table rather than spelled again here: the
    /// form's labels and a failing run's 「ステップ …」 have to agree, and two
    /// copies of the table would let them drift apart without anything saying
    /// so.
    fn prefix(&self) -> &'static str {
        self.runner_list().prefix()
    }

    fn runner_list(&self) -> MacroStepList {
        match self {
            StepList::Main => MacroStepList::Main,
            StepList::Else => MacroStepList::Else,
            StepList::Timeout => MacroStepList::Timeout,
        }
    }
}

/// Where a row sits in the form: one (list, index) pair per level, outermost
/// first. The form unfolds every level, so this is a path rather than the
/// parent/child pair of the one-level-deep form it replaces. The list of the
/// first segment is always [`StepList::Main`], the macro's own steps.
#[derive(Clone, PartialEq, Eq, Debug)]
struct StepPath(Rc<Vec<(StepList, usize)>>);

impl StepPath {
    fn top(index: usize) -> Self {
        StepPath(Rc::new(vec![(StepList::Main, index)]))
    }

    fn child(&self, list: StepList, index: usize) -> Self {
        let mut segments = Vec::with_capacity(self.0.len() + 1);
        segments.extend_from_slice(&self.0);
        segments.push((list, index));
        StepPath(Rc::new(segments))
    }

    fn index(&self) -> usize {
        self.0.last().map(|(_, index)| *index).unwrap_or(0)
    }

    fn depth(&self) -> usize {
        self.0.len()
    }

    /// 「3」 for a top-level step, 「3-2-1」 for the first step of the 繰り返し
    /// that is the second step of the macro, 「3-e2」 for the second step of
    /// the 「それ以外」 side of the 分岐 that is the third step of the macro.
    /// Matches how a failing run names the step it stopped on
    /// ([`terminal::terminal_macro::MacroStepList`]): `Else` and `Timeout`
    /// levels carry a letter so the two sides of a 分岐 are never numbered
    /// alike.
    fn label(&self) -> String {
        self.0
            .iter()
            .map(|(list, index)| format!("{}{}", list.prefix(), index + 1))
            .collect::<Vec<_>>()
            .join("-")
    }

    /// Names the drawn row for [`gpui::VisualTestContext::debug_bounds`].
    fn debug_selector(&self) -> String {
        let mut selector = "macro-step".to_string();
        for (list, index) in self.0.iter() {
            selector.push('-');
            selector.push_str(list.prefix());
            selector.push_str(&(index + 1).to_string());
        }
        selector
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
    /// 取り込む変数名 of a 期待文字を待つ or a 分岐 row. Empty means "capture
    /// nothing".
    capture: Entity<Editor>,
    regex: bool,
    newline: bool,
    /// Which part of the output a 分岐 row decides on.
    scope: MatchScope,
    /// Whether a 期待文字を待つ row runs `timeout_children` instead of stopping
    /// the macro. Kept apart from that list being empty, because 「何もせずに
    /// 次へ進む」 is a deliberate answer and has to survive a save.
    continue_on_timeout: bool,
    /// Steps of a 繰り返し row, or the 「そのとき」 side of a 分岐 row.
    children: Vec<StepRow>,
    /// The 「それ以外」 side of a 分岐 row.
    else_children: Vec<StepRow>,
    /// The 「タイムアウト時」 steps of a 期待文字を待つ row.
    timeout_children: Vec<StepRow>,
}

impl StepRow {
    fn new(step: &MacroStep, window: &mut Window, cx: &mut App) -> Self {
        let rows = |steps: &[MacroStep], window: &mut Window, cx: &mut App| {
            steps
                .iter()
                .map(|step| StepRow::new(step, window, cx))
                .collect::<Vec<_>>()
        };

        let kind = match step {
            MacroStep::Send { .. } => StepKind::Send,
            MacroStep::WaitFor { .. } => StepKind::WaitFor,
            MacroStep::Sleep { .. } => StepKind::Sleep,
            MacroStep::Clear => StepKind::Clear,
            MacroStep::Repeat { .. } => StepKind::Repeat,
            MacroStep::If { .. } => StepKind::If,
        };
        let text_value = match step {
            MacroStep::Send { text, .. } => text.clone(),
            MacroStep::WaitFor { pattern, .. } | MacroStep::If { pattern, .. } => pattern.clone(),
            _ => String::new(),
        };
        let number_value = match step {
            MacroStep::WaitFor { timeout_ms, .. } => format_seconds(*timeout_ms),
            MacroStep::Sleep { ms } => format_seconds(*ms),
            _ => String::new(),
        };
        let count_value = match step {
            MacroStep::Repeat { count, .. } => count.to_string(),
            _ => "1".to_string(),
        };
        let regex = match step {
            MacroStep::WaitFor { regex, .. } | MacroStep::If { regex, .. } => *regex,
            _ => false,
        };
        let newline = match step {
            MacroStep::Send { newline, .. } => *newline,
            _ => true,
        };
        let scope = match step {
            MacroStep::If { scope, .. } => *scope,
            _ => MatchScope::default(),
        };
        let capture_value = match step {
            MacroStep::WaitFor { capture, .. } | MacroStep::If { capture, .. } => {
                capture.clone().unwrap_or_default()
            }
            _ => String::new(),
        };
        let continue_on_timeout = matches!(
            step,
            MacroStep::WaitFor {
                on_timeout: Some(_),
                ..
            }
        );
        let children = match step {
            MacroStep::Repeat { steps, .. } => rows(steps, window, cx),
            MacroStep::If { then_steps, .. } => rows(then_steps, window, cx),
            _ => Vec::new(),
        };
        let else_children = match step {
            MacroStep::If { else_steps, .. } => rows(else_steps, window, cx),
            _ => Vec::new(),
        };
        let timeout_children = match step {
            MacroStep::WaitFor {
                on_timeout: Some(steps),
                ..
            } => rows(steps, window, cx),
            _ => Vec::new(),
        };

        let single_line =
            |value: String, placeholder: &'static str, window: &mut Window, cx: &mut App| {
                cx.new(|cx| {
                    let mut editor = Editor::single_line(window, cx);
                    editor.set_text(value, window, cx);
                    if !placeholder.is_empty() {
                        editor.set_placeholder_text(placeholder, window, cx);
                    }
                    editor
                })
            };
        let text = single_line(text_value, kind.text_placeholder(), window, cx);
        let number = single_line(number_value, "", window, cx);
        let count = single_line(count_value, "", window, cx);
        let capture = single_line(capture_value, CAPTURE_PLACEHOLDER, window, cx);

        Self {
            kind,
            text,
            number,
            count,
            capture,
            regex,
            newline,
            scope,
            continue_on_timeout,
            children,
            else_children,
            timeout_children,
        }
    }

    /// The row's identity for element ids: stable while the row lives, and
    /// carried along when the row is moved.
    fn element_id(&self) -> gpui::EntityId {
        self.text.entity_id()
    }

    fn list(&self, list: StepList) -> &Vec<StepRow> {
        match list {
            StepList::Main => &self.children,
            StepList::Else => &self.else_children,
            StepList::Timeout => &self.timeout_children,
        }
    }

    fn list_mut(&mut self, list: StepList) -> &mut Vec<StepRow> {
        match list {
            StepList::Main => &mut self.children,
            StepList::Else => &mut self.else_children,
            StepList::Timeout => &mut self.timeout_children,
        }
    }

    fn to_step(&self, label: &str, cx: &App) -> Result<MacroStep, String> {
        let text = self.text.read(cx).text(cx);
        // `list` names which child list `rows` is, so a step inside 「それ以外」
        // or 「タイムアウト時」 gets a label ("2-e1", "2-t1") that cannot be
        // confused with the same position in another list — the same spelling
        // [`terminal::terminal_macro::MacroRunner::step_label`] uses.
        let steps = |rows: &[StepRow], list: StepList, cx: &App| {
            let prefix = list.prefix();
            let mut steps = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                steps.push(row.to_step(&format!("{label}-{prefix}{}", index + 1), cx)?);
            }
            Ok::<_, String>(steps)
        };
        let capture = |field: &Entity<Editor>, cx: &App| -> Result<Option<String>, String> {
            let capture = field.read(cx).text(cx).trim().to_string();
            if capture.contains(['$', '{', '}']) {
                return Err(format!(
                    "ステップ {label}: 取り込む変数名に $ {{ }} は使えません。"
                ));
            }
            Ok((!capture.is_empty()).then_some(capture))
        };
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
                    capture: capture(&self.capture, cx)?,
                    on_timeout: self
                        .continue_on_timeout
                        .then(|| steps(&self.timeout_children, StepList::Timeout, cx))
                        .transpose()?,
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
                Ok(MacroStep::Repeat {
                    count,
                    steps: steps(&self.children, StepList::Main, cx)?,
                })
            }
            StepKind::If => {
                if text.trim().is_empty() {
                    return Err(format!(
                        "ステップ {label}: 判定する文字列を入力してください。"
                    ));
                }
                if self.regex && regex::Regex::new(&text).is_err() {
                    return Err(format!("ステップ {label}: 正規表現が不正です。"));
                }
                Ok(MacroStep::If {
                    pattern: text,
                    regex: self.regex,
                    scope: self.scope,
                    capture: capture(&self.capture, cx)?,
                    then_steps: steps(&self.children, StepList::Main, cx)?,
                    else_steps: steps(&self.else_children, StepList::Else, cx)?,
                })
            }
        }
    }
}

const CAPTURE_PLACEHOLDER: &str = "省略可";

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

/// Stands in for the macros of a folder the user made but has not filled yet.
const EMPTY_FOLDER_ROW: &str = "マクロがありません — ここに追加";

/// The form's folder dropdown entry for 未分類.
const NO_FOLDER: &str = "(フォルダーなし)";

/// A macro dragged from the tree onto a folder heading.
#[derive(Clone)]
struct DraggedMacro {
    key: MacroKey,
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
    /// Placeholder under a folder that holds nothing yet — a state ADR-F35
    /// made possible, and the only invitation to put something in it.
    EmptyFolder { folder: String },
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

/// Which of `file.connections` run the macro `key` names, one flag per entry so
/// the caller can rewrite them while walking the same list.
///
/// A connection saved after ADR-F35 names the folder as well; one written
/// before it carries the name alone and is matched through
/// [`TerminalSessionsFile::macro_for_connection`], which is the same reading
/// the runner uses. Without that, renaming or moving a macro would leave the
/// older reference behind and 「接続後に実行するマクロ」 would silently stop
/// running — or worse, latch onto another folder's macro of the same name.
///
/// Must be called *before* the macros are rewritten: a name-only reference only
/// resolves against the file as it stands.
fn connections_running(file: &TerminalSessionsFile, key: &MacroKey) -> Vec<bool> {
    file.connections
        .iter()
        .map(|connection| {
            if connection.macro_folder.is_some() {
                return connection.macro_folder == key.0
                    && connection.macro_name.as_deref() == Some(key.1.as_str());
            }
            file.macro_for_connection(connection)
                .is_some_and(|resolved| resolved.is(key.0.as_deref(), &key.1))
        })
        .collect()
}

/// Which of `file.connections` run a macro filed under `folder`, resolved the
/// same way as [`connections_running`]. A `macro_folder` naming the folder
/// counts even when nothing carries the name any more, so deleting the folder
/// cannot leave a reference that `ensure_macro_folders` would resurrect it from.
fn connections_in_folder(file: &TerminalSessionsFile, folder: &str) -> Vec<bool> {
    file.connections
        .iter()
        .map(|connection| match connection.macro_folder.as_deref() {
            Some(named) => named == folder,
            None => file
                .macro_for_connection(connection)
                .is_some_and(|resolved| resolved.folder.as_deref() == Some(folder)),
        })
        .collect()
}

/// The raw text of every field on the form. Comparing one of these against the
/// snapshot taken when the macro was loaded is how the editor knows the form
/// holds unsaved work: raw text rather than a parsed [`TerminalMacro`], so a
/// half-typed number counts as a change too.
#[derive(PartialEq, Eq)]
struct FormSnapshot {
    name: String,
    folder: Option<String>,
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
    capture: String,
    regex: bool,
    newline: bool,
    scope: MatchScope,
    continue_on_timeout: bool,
    children: Vec<StepSnapshot>,
    else_children: Vec<StepSnapshot>,
    timeout_children: Vec<StepSnapshot>,
}

impl StepRow {
    fn snapshot(&self, cx: &App) -> StepSnapshot {
        let snapshots = |rows: &[StepRow]| rows.iter().map(|row| row.snapshot(cx)).collect();
        StepSnapshot {
            kind: self.kind,
            text: self.text.read(cx).text(cx),
            number: self.number.read(cx).text(cx),
            count: self.count.read(cx).text(cx),
            capture: self.capture.read(cx).text(cx),
            regex: self.regex,
            newline: self.newline,
            scope: self.scope,
            continue_on_timeout: self.continue_on_timeout,
            children: snapshots(&self.children),
            else_children: snapshots(&self.else_children),
            timeout_children: snapshots(&self.timeout_children),
        }
    }
}

/// The inline 「新しいフォルダー」 name field. Both the tree's toolbar button and
/// the form's folder dropdown open one; `file_open_macro_in_it` decides whether
/// the folder that comes out also becomes the open macro's folder.
struct NewFolderEntry {
    field: Entity<Editor>,
    file_open_macro_in_it: bool,
}

pub struct MacroEditor {
    store: Entity<TerminalSessionStore>,
    /// Folder and name the macro had when the form was last loaded, so a rename
    /// or a move replaces the original entry instead of adding a second one.
    /// The name alone would not do: two folders may hold the same one.
    original_key: Option<MacroKey>,
    name: Entity<Editor>,
    /// Folder picked in the form's dropdown; `None` is 未分類. A dropdown rather
    /// than a text field, so a macro can only ever be filed under a folder that
    /// exists (ADR-F35).
    form_folder: Option<String>,
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
    /// Whether the [`UNFILED_FOLDER`] heading is folded shut. A real folder
    /// keeps that in the file; this one has nowhere to live, so it lasts as
    /// long as the modal.
    unfiled_collapsed: bool,
    /// Folder whose name is being edited in the tree, with the field holding
    /// the new name.
    renaming_folder: Option<(String, Entity<Editor>)>,
    new_folder: Option<NewFolderEntry>,
    steps: Vec<StepRow>,
    error: Option<SharedString>,
    /// Macro whose 削除 has been asked for but not yet confirmed.
    pending_delete: Option<MacroKey>,
    /// Folder whose 削除 has been asked for and which still holds macros, so
    /// the user has to say what becomes of them.
    pending_folder_delete: Option<String>,
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
            original_key: None,
            name,
            form_folder: None,
            tags,
            description,
            repeat,
            repeat_interval,
            list_filter,
            saved_snapshot: None,
            unfiled_collapsed: false,
            renaming_folder: None,
            new_folder: None,
            steps: Vec::new(),
            error: None,
            pending_delete: None,
            pending_folder_delete: None,
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
        // A macro with no name yet stands for nothing on disk, which is how the
        // tree hands the form 「このフォルダーに新しいマクロ」.
        self.original_key = (!edited.name.is_empty()).then(|| edited.key());
        self.name.update(cx, |editor, cx| {
            editor.set_text(edited.name, window, cx);
        });
        self.form_folder = edited.folder.clone();
        self.new_folder = None;
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
            .map(|step| StepRow::new(step, window, cx))
            .collect();
        self.error = None;
        self.pending_delete = None;
        self.pending_folder_delete = None;
        self.pending_selection = None;
        self.saved_snapshot = Some(self.snapshot(cx));
        window.focus(&self.name.focus_handle(cx), cx);
        cx.notify();
    }

    fn snapshot(&self, cx: &App) -> FormSnapshot {
        FormSnapshot {
            name: self.name.read(cx).text(cx),
            folder: self.form_folder.clone(),
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
            && !edited.name.is_empty()
            && self.original_key.as_ref() == Some(&edited.key())
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
        self.original_key = None;
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

    /// `名前 のコピー`, numbered when that is taken *in the same folder*, so a
    /// copy never silently overwrites the macro it was made from. A folder of
    /// its own is what makes 「A-PC用/ls」 and 「B-PC用/ls」 both keep their name.
    fn unused_copy_name(&self, base: &str, cx: &App) -> String {
        let base = base.trim();
        let base = if base.is_empty() { "マクロ" } else { base };
        let folder = self.form_folder.as_deref();
        let taken = |candidate: &str| self.store.read(cx).macro_named(folder, candidate).is_some();
        let candidate = format!("{base} のコピー");
        if !taken(&candidate) {
            return candidate;
        }
        // There are only as many taken names as there are macros, so one of
        // these suffixes is always free.
        let limit = self.store.read(cx).macros().len() + 2;
        for suffix in 2..=limit {
            let numbered = format!("{base} のコピー {suffix}");
            if !taken(&numbered) {
                return numbered;
            }
        }
        candidate
    }

    /// `name` if `folder` does not hold it yet, otherwise 「name (2)」, 「name
    /// (3)」… — what a macro dragged into a folder that already has one by that
    /// name is renamed to.
    fn unused_name_in_folder(&self, folder: Option<&str>, name: &str, cx: &App) -> String {
        if self.store.read(cx).macro_named(folder, name).is_none() {
            return name.to_string();
        }
        let limit = self.store.read(cx).macros().len() + 2;
        for suffix in 2..=limit {
            let numbered = format!("{name} ({suffix})");
            if self.store.read(cx).macro_named(folder, &numbered).is_none() {
                return numbered;
            }
        }
        name.to_string()
    }

    fn macro_folders(&self, cx: &App) -> Vec<MacroFolder> {
        self.store.read(cx).macro_folders().to_vec()
    }

    /// The saved macros the tree shows, in folder-then-name order. Read
    /// straight from the store so the tree is whatever is on disk right now,
    /// not a copy taken at open time.
    fn listed_macros(&self, cx: &App) -> Vec<TerminalMacro> {
        let folders = self.macro_folders(cx);
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros, &folders);
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

    /// Folder names in the order the user arranged them, which is what both the
    /// tree and the form's dropdown show.
    fn known_folders(&self, cx: &App) -> Vec<String> {
        self.macro_folders(cx)
            .into_iter()
            .map(|folder| folder.name)
            .collect()
    }

    /// The tree the left pane draws: every folder gets a heading, in order,
    /// whether or not anything is filed under it, and 未分類 comes last. With no
    /// folders at all the list stays flat, so a user who never made one is not
    /// shown a grouping they did not ask for.
    fn macro_tree(&self, cx: &App) -> Vec<MacroTreeRow> {
        let folders = self.macro_folders(cx);
        let filtering = !self.list_filter.read(cx).text(cx).trim().is_empty();
        let listed = self.listed_macros(cx);

        let mut rows = Vec::new();
        for folder in &folders {
            let entries = listed
                .iter()
                .filter(|terminal_macro| terminal_macro.folder.as_deref() == Some(&folder.name))
                .cloned()
                .collect::<Vec<_>>();
            // While the filter narrows the list, a folder with no match has
            // nothing to say; unfiltered, an empty folder is shown so it can be
            // filled, renamed or deleted.
            if filtering && entries.is_empty() {
                continue;
            }
            // A folded heading must never be the reason a match is not drawn,
            // which is the same rule the runner picker follows: the count in
            // the header and the rows under it have to agree.
            let collapsed = folder.collapsed && !filtering;
            rows.push(MacroTreeRow::Folder {
                folder: Some(folder.name.clone()),
                collapsed,
                count: entries.len(),
            });
            if collapsed {
                continue;
            }
            if entries.is_empty() {
                rows.push(MacroTreeRow::EmptyFolder {
                    folder: folder.name.clone(),
                });
            } else {
                rows.extend(entries.into_iter().map(MacroTreeRow::Macro));
            }
        }

        // A macro whose folder is not registered would otherwise fall out of
        // the tree entirely; `ensure_macro_folders` normally prevents that, but
        // the tree must not be the place where a hand-edited file loses rows.
        let unfiled = listed
            .into_iter()
            .filter(|terminal_macro| {
                terminal_macro
                    .folder
                    .as_ref()
                    .is_none_or(|folder| !folders.iter().any(|candidate| candidate.name == *folder))
            })
            .collect::<Vec<_>>();
        if unfiled.is_empty() {
            return rows;
        }
        if folders.is_empty() {
            rows.extend(unfiled.into_iter().map(MacroTreeRow::Macro));
            return rows;
        }
        let unfiled_collapsed = self.unfiled_collapsed && !filtering;
        rows.push(MacroTreeRow::Folder {
            folder: None,
            collapsed: unfiled_collapsed,
            count: unfiled.len(),
        });
        if !unfiled_collapsed {
            rows.extend(unfiled.into_iter().map(MacroTreeRow::Macro));
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
                capture: None,
                on_timeout: None,
            },
            StepKind::Sleep => MacroStep::Sleep { ms: 1000 },
            StepKind::Clear => MacroStep::Clear,
            StepKind::Repeat => MacroStep::Repeat {
                count: 1,
                steps: Vec::new(),
            },
            StepKind::If => MacroStep::If {
                pattern: String::new(),
                regex: false,
                scope: MatchScope::default(),
                capture: None,
                then_steps: Vec::new(),
                else_steps: Vec::new(),
            },
        }
    }

    fn add_step(&mut self, kind: StepKind, window: &mut Window, cx: &mut Context<Self>) {
        let step = Self::blank_step(kind);
        self.steps.push(StepRow::new(&step, window, cx));
        cx.notify();
    }

    /// Appends a step to one of the child lists of the row at `parent`.
    fn add_child_step(
        &mut self,
        parent: &StepPath,
        list: StepList,
        kind: StepKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = Self::blank_step(kind);
        let row = StepRow::new(&step, window, cx);
        let Some(parent_row) = self.row_mut(parent) else {
            return;
        };
        parent_row.list_mut(list).push(row);
        cx.notify();
    }

    fn row(&self, path: &StepPath) -> Option<&StepRow> {
        let mut segments = path.0.iter();
        let (_, first) = segments.next()?;
        let mut row = self.steps.get(*first)?;
        for (list, index) in segments {
            row = row.list(*list).get(*index)?;
        }
        Some(row)
    }

    fn row_mut(&mut self, path: &StepPath) -> Option<&mut StepRow> {
        let mut segments = path.0.iter();
        let (_, first) = segments.next()?;
        let mut row = self.steps.get_mut(*first)?;
        for (list, index) in segments {
            row = row.list_mut(*list).get_mut(*index)?;
        }
        Some(row)
    }

    /// The list `path` is an element of, so that reordering and deleting work
    /// the same at every level.
    fn siblings_mut(&mut self, path: &StepPath) -> Option<&mut Vec<StepRow>> {
        let (last, parent) = path.0.split_last()?;
        if parent.is_empty() {
            return Some(&mut self.steps);
        }
        let parent = StepPath(Rc::new(parent.to_vec()));
        Some(self.row_mut(&parent)?.list_mut(last.0))
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

    /// Points the form's folder dropdown at `folder` without that counting as
    /// an unsaved edit, for when the tree moved or renamed the macro on show.
    fn set_form_folder(&mut self, folder: Option<String>) {
        self.form_folder = folder.clone();
        if let Some(snapshot) = &mut self.saved_snapshot {
            snapshot.folder = folder;
        }
    }

    /// Points the 削除しますか / 破棄しますか prompts at wherever the macro they
    /// name has just gone. `remap` answers with the macro's new key, or `None`
    /// when it is no longer there — a prompt left standing for an entry that
    /// moved would otherwise act on the wrong macro, or on none at all.
    fn remap_pending_macro(&mut self, remap: impl Fn(&MacroKey) -> Option<MacroKey>) {
        if let Some(key) = self.pending_delete.take() {
            self.pending_delete = remap(&key);
        }
        // The inner `None` is 新規, which no rename or move can invalidate.
        let Some(Some(edited)) = self.pending_selection.clone() else {
            return;
        };
        self.pending_selection = remap(&edited.key()).map(|(folder, name)| {
            Some(TerminalMacro {
                folder,
                name,
                ..edited
            })
        });
    }

    /// Folds a heading. A real folder's state goes into the file so the runner
    /// picker and the next session see it; 未分類 has no folder to keep it in.
    fn toggle_folder_collapsed(&mut self, folder: Option<String>, cx: &mut Context<Self>) {
        let Some(folder) = folder else {
            self.unfiled_collapsed = !self.unfiled_collapsed;
            cx.notify();
            return;
        };
        let collapsed = self
            .macro_folders(cx)
            .iter()
            .any(|candidate| candidate.name == folder && candidate.collapsed);
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
        let store = self.store.clone();
        if let Some(write) = write_folder_collapsed(&store, &folder, !collapsed, cx) {
            self.report_write_failure(write, "フォルダーの開閉を保存", cx);
        }
        cx.notify();
    }

    /// Opens the inline 「新しいフォルダー」 field. `file_open_macro_in_it` is set
    /// when the request came from the form's dropdown, where creating a folder
    /// is also a way of choosing it.
    fn begin_new_folder(
        &mut self,
        file_open_macro_in_it: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("フォルダー名", window, cx);
            editor
        });
        window.focus(&field.focus_handle(cx), cx);
        self.new_folder = Some(NewFolderEntry {
            field,
            file_open_macro_in_it,
        });
        self.renaming_folder = None;
        self.error = None;
        cx.notify();
    }

    fn confirm_new_folder(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.new_folder.take() else {
            return;
        };
        let Some(name) = normalize_folder(&entry.field.read(cx).text(cx)) else {
            self.error = Some("フォルダー名を入力してください。".into());
            self.new_folder = Some(entry);
            cx.notify();
            return;
        };
        // Writing before the first read would rewrite the file from an empty
        // in-memory copy, dropping every macro and connection already in it.
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            self.new_folder = Some(entry);
            cx.notify();
            return;
        }
        if self.store.read(cx).file().has_folder(&name) {
            self.error = Some("同じ名前のフォルダーがあります。".into());
            self.new_folder = Some(entry);
            cx.notify();
            return;
        }

        let mut file = self.store.read(cx).file().clone();
        file.macro_folders.push(MacroFolder::new(name.clone()));
        if entry.file_open_macro_in_it {
            self.form_folder = Some(name);
        }
        self.error = None;
        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダーを作成", cx);
        cx.notify();
    }

    /// Moves a folder one place up or down, which is all the order the tree and
    /// the picker follow: the position in `macro_folders` is the display order.
    fn move_folder(&mut self, folder: &str, up: bool, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
        let mut file = self.store.read(cx).file().clone();
        let Some(index) = file.folder_index(folder) else {
            return;
        };
        let Some(target) = (if up {
            index.checked_sub(1)
        } else {
            (index + 1 < file.macro_folders.len()).then_some(index + 1)
        }) else {
            return;
        };
        file.macro_folders.swap(index, target);
        self.error = None;
        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダーを移動", cx);
        cx.notify();
    }

    /// 削除… on a folder: an empty one goes straight away, one that still holds
    /// macros has to be answered for first.
    fn request_folder_delete(
        &mut self,
        folder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let empty = self
            .store
            .read(cx)
            .file()
            .macros_in_folder(Some(&folder))
            .next()
            .is_none();
        if empty {
            self.delete_folder(&folder, true, window, cx);
            return;
        }
        self.pending_folder_delete = Some(folder);
        self.error = None;
        cx.notify();
    }

    /// Removes `folder`. `keep_macros` files what was in it under 未分類;
    /// otherwise the macros go with it.
    fn delete_folder(
        &mut self,
        folder: &str,
        keep_macros: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_folder_delete = None;
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }

        let mut file = self.store.read(cx).file().clone();
        // Worked out while the macros are still in place, because a connection
        // written before ADR-F35 only names one and has to be resolved.
        let running = connections_in_folder(&file, folder);
        file.macro_folders
            .retain(|candidate| candidate.name != folder);
        let open_in_folder = self
            .original_key
            .as_ref()
            .is_some_and(|(open_folder, _)| open_folder.as_deref() == Some(folder));

        if !keep_macros {
            file.macros
                .retain(|terminal_macro| terminal_macro.folder.as_deref() != Some(folder));
            // The macros a connection pointed at are gone, so the reference has
            // to go too — a stale `macro_folder` would put the folder straight
            // back on the next load (`ensure_macro_folders`), and a stale name
            // would attach the connection to some other folder's macro.
            for (connection, runs) in file.connections.iter_mut().zip(running) {
                if runs {
                    connection.macro_name = None;
                    connection.macro_folder = None;
                }
            }
            self.remap_pending_macro(|(pending_folder, name)| {
                (pending_folder.as_deref() != Some(folder))
                    .then(|| (pending_folder.clone(), name.clone()))
            });
            if open_in_folder {
                // The macro the form was showing has just gone away.
                self.load_macro(None, window, cx);
            } else if self.form_folder.as_deref() == Some(folder) {
                self.set_form_folder(None);
            }
            self.error = None;
            let write = self.store.update(cx, |store, cx| store.write(file, cx));
            self.report_write_failure(write, "フォルダーを削除", cx);
            cx.notify();
            return;
        }

        // 未分類 may already hold one of these names, and identity is the
        // (folder, name) pair, so an unfiled macro that would collide is
        // numbered rather than made into a second entry with the same key.
        let mut taken = file
            .macros
            .iter()
            .filter(|candidate| candidate.folder.is_none())
            .map(|candidate| candidate.name.clone())
            .collect::<Vec<_>>();
        let limit = file.macros.len() + 2;
        let mut renamed: Vec<(String, String)> = Vec::new();
        for terminal_macro in &mut file.macros {
            if terminal_macro.folder.as_deref() != Some(folder) {
                continue;
            }
            let mut name = terminal_macro.name.clone();
            if taken.contains(&name) {
                for suffix in 2..=limit {
                    let numbered = format!("{name} ({suffix})");
                    if !taken.contains(&numbered) {
                        name = numbered;
                        break;
                    }
                }
            }
            if name != terminal_macro.name {
                renamed.push((terminal_macro.name.clone(), name.clone()));
                terminal_macro.name = name.clone();
            }
            taken.push(name);
            terminal_macro.folder = None;
        }
        let renamed_to = |name: &str| {
            renamed
                .iter()
                .find(|(from, _)| from == name)
                .map(|(_, to)| to.clone())
        };
        // The macros are still there, so the connections pointing at them keep
        // working once the folder half of the reference is dropped.
        for (connection, runs) in file.connections.iter_mut().zip(running) {
            if !runs {
                continue;
            }
            if let Some(name) = connection.macro_name.as_deref()
                && let Some(renamed) = renamed_to(name)
            {
                connection.macro_name = Some(renamed);
            }
            connection.macro_folder = None;
        }
        self.remap_pending_macro(|(pending_folder, name)| {
            Some(if pending_folder.as_deref() == Some(folder) {
                (None, renamed_to(name).unwrap_or_else(|| name.clone()))
            } else {
                (pending_folder.clone(), name.clone())
            })
        });

        if open_in_folder {
            if let Some((_, open_name)) = self.original_key.take() {
                let open_name = renamed_to(&open_name).unwrap_or(open_name);
                self.name.update(cx, |editor, cx| {
                    editor.set_text(open_name.clone(), window, cx);
                });
                if let Some(snapshot) = &mut self.saved_snapshot {
                    snapshot.name = open_name.clone();
                }
                self.original_key = Some((None, open_name));
            }
            self.set_form_folder(None);
        } else if self.form_folder.as_deref() == Some(folder) {
            self.set_form_folder(None);
        }
        self.error = None;

        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダーを削除", cx);
        cx.notify();
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
        self.new_folder = None;
        self.error = None;
        cx.notify();
    }

    /// Renames a folder: the [`MacroFolder`] itself, the `folder` of every
    /// macro filed under it and the `macro_folder` of every connection that
    /// points into it. Sub-folders travel with their parent, so renaming 「拠点」
    /// also renames 「拠点/本社」.
    fn confirm_folder_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
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
        // The rename carries the sub-folders with it, so checking the typed
        // name alone is not enough: 「拠点」 → 「全社」 also turns 「拠点/本社」 into
        // 「全社/本社」, which a folder already called that would collide with —
        // two `macro_folders` entries by the same name, and two macros sharing
        // one `MacroKey`.
        let after_rename = self
            .store
            .read(cx)
            .macro_folders()
            .iter()
            .map(|folder| {
                (
                    folder.name.clone(),
                    folder_after_rename(&folder.name, &from, &renamed),
                )
            })
            .collect::<Vec<_>>();
        let collides = after_rename.iter().any(|(name, after)| {
            let Some(after) = after else {
                return false;
            };
            after_rename.iter().any(|(other, other_after)| {
                other != name && other_after.as_ref().unwrap_or(other) == after
            })
        });
        if collides || self.store.read(cx).file().has_folder(&renamed) {
            self.error = Some("同じ名前のフォルダーがあります。".into());
            self.renaming_folder = Some((from, field));
            cx.notify();
            return;
        }

        let mut file = self.store.read(cx).file().clone();
        let mut touched = false;
        for folder in &mut file.macro_folders {
            if let Some(name) = folder_after_rename(&folder.name, &from, &renamed) {
                folder.name = name;
                touched = true;
            }
        }
        for terminal_macro in &mut file.macros {
            let Some(folder) = terminal_macro.folder.as_deref() else {
                continue;
            };
            if let Some(folder) = folder_after_rename(folder, &from, &renamed) {
                terminal_macro.folder = Some(folder);
                touched = true;
            }
        }
        for connection in &mut file.connections {
            let Some(folder) = connection.macro_folder.as_deref() else {
                continue;
            };
            if let Some(folder) = folder_after_rename(folder, &from, &renamed) {
                connection.macro_folder = Some(folder);
                touched = true;
            }
        }
        if !touched {
            cx.notify();
            return;
        }

        if let Some(key) = self.original_key.take() {
            let folder = key
                .0
                .map(|folder| folder_after_rename(&folder, &from, &renamed).unwrap_or(folder));
            self.original_key = Some((folder, key.1));
        }
        self.remap_pending_macro(|(pending_folder, name)| {
            let folder = pending_folder.as_ref().map(|folder| {
                folder_after_rename(folder, &from, &renamed).unwrap_or_else(|| folder.clone())
            });
            Some((folder, name.clone()))
        });
        if let Some(folder) = self.form_folder.clone()
            && let Some(folder) = folder_after_rename(&folder, &from, &renamed)
        {
            self.set_form_folder(Some(folder));
        }
        self.error = None;

        let write = self.store.update(cx, |store, cx| store.write(file, cx));
        self.report_write_failure(write, "フォルダー名を変更", cx);
        cx.notify();
    }

    /// Files the macro `key` names under `folder` (`None` for
    /// [`UNFILED_FOLDER`]), which is what a drag from the tree onto a heading
    /// does. A folder that already holds that name keeps both: the moved macro
    /// gets a numbered name rather than overwriting what is there.
    fn move_macro_to_folder(
        &mut self,
        key: &MacroKey,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }
        if key.0 == folder {
            return;
        }
        let renamed = self.unused_name_in_folder(folder.as_deref(), &key.1, cx);

        let mut file = self.store.read(cx).file().clone();
        // Worked out before the macro moves, so a connection that names it
        // without a folder still resolves.
        let running = connections_running(&file, key);
        let Some(moved) = file
            .macros
            .iter_mut()
            .find(|candidate| candidate.is(key.0.as_deref(), &key.1))
        else {
            return;
        };
        moved.folder = folder.clone();
        moved.name = renamed.clone();
        // The connection that ran it should keep running it after the move.
        for (connection, runs) in file.connections.iter_mut().zip(running) {
            if runs {
                connection.macro_name = Some(renamed.clone());
                connection.macro_folder = folder.clone();
            }
        }

        let moved_key = (folder.clone(), renamed.clone());
        self.remap_pending_macro(|candidate| {
            Some(if candidate == key {
                moved_key.clone()
            } else {
                candidate.clone()
            })
        });
        if self.original_key.as_ref() == Some(key) {
            self.original_key = Some((folder.clone(), renamed.clone()));
            self.set_form_folder(folder);
            // A move that had to number the name must show that name, or the
            // next 保存 would file the macro under the old one all over again.
            if renamed != key.1 {
                self.name.update(cx, |editor, cx| {
                    editor.set_text(renamed.clone(), window, cx);
                });
                if let Some(snapshot) = &mut self.saved_snapshot {
                    snapshot.name = renamed;
                }
            }
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
            name,
            folder: self.form_folder.clone(),
            tags: normalize_tags(&self.tags.read(cx).text(cx)),
            description: self.description.read(cx).text(cx).trim().to_string(),
            repeat,
            repeat_interval_ms,
            steps,
        };

        // Identity is the (folder, name) pair, so the same name in another
        // folder is a different macro and must not be written over — that is
        // exactly what 「A-PC用/ls」 and 「B-PC用/ls」 rely on.
        let key = terminal_macro.key();
        let original_key = self.original_key.clone();
        let taken = self
            .store
            .read(cx)
            .file()
            .macros
            .iter()
            .position(|candidate| candidate.key() == key);
        if taken.is_some() && original_key.as_ref() != Some(&key) {
            self.error = Some("同じフォルダーに同名のマクロがあります。".into());
            cx.notify();
            return;
        }

        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            // Which connections run the macro has to be settled before it is
            // rewritten: a reference written before ADR-F35 carries the name
            // alone and only resolves against the file as it stands.
            let running = original_key
                .as_ref()
                .filter(|original| **original != key)
                .map(|original| connections_running(&file, original));
            let existing = taken.or_else(|| {
                original_key.as_ref().and_then(|original| {
                    file.macros
                        .iter()
                        .position(|candidate| candidate.key() == *original)
                })
            });
            match existing {
                Some(index) => file.macros[index] = terminal_macro,
                None => file.macros.push(terminal_macro),
            }
            // A rename or a move has to take the connections pointing at the
            // macro with it, or 「接続後に実行するマクロ」 silently stops running.
            if let Some(running) = running {
                for (connection, runs) in file.connections.iter_mut().zip(running) {
                    if runs {
                        connection.macro_name = Some(key.1.clone());
                        connection.macro_folder = key.0.clone();
                    }
                }
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

    fn request_delete(&mut self, key: MacroKey, cx: &mut Context<Self>) {
        self.pending_delete = Some(key);
        self.error = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.pending_delete.take() else {
            return;
        };
        if !self.store.read(cx).is_loaded() {
            self.error = Some(LOADING_ERROR.into());
            cx.notify();
            return;
        }

        let write = self.store.update(cx, |store, cx| {
            let mut file = store.file().clone();
            // Settled before the macro goes, so a name-only reference still
            // resolves. Leaving it behind would make 「接続後に実行するマクロ」
            // either do nothing or, once the name exists in another folder,
            // run that one instead.
            let running = connections_running(&file, &key);
            file.macros
                .retain(|candidate| !candidate.is(key.0.as_deref(), &key.1));
            for (connection, runs) in file.connections.iter_mut().zip(running) {
                if runs {
                    connection.macro_name = None;
                    connection.macro_folder = None;
                }
            }
            store.write(file, cx)
        });
        self.remap_pending_macro(|candidate| (*candidate != key).then(|| candidate.clone()));
        // The form was showing the macro that just went away, so put it back to
        // a blank new macro rather than leaving a rename of a deleted entry.
        if self.original_key.as_ref() == Some(&key) {
            self.load_macro(None, window, cx);
        }
        cx.notify();

        self.report_write_failure(write, "削除", cx);
    }

    /// Opens a blank form already filed under `folder` — what 「新しいマクロ」 on
    /// a folder heading and the empty-folder placeholder both do.
    fn new_macro_in_folder(
        &mut self,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_macro(
            Some(TerminalMacro {
                folder,
                ..TerminalMacro::default()
            }),
            window,
            cx,
        );
    }

    /// The left pane: every folder as a heading, in the order the user
    /// arranged them, the macros filed under it, and 未分類 last. Headings
    /// collapse, rename, reorder, take drops and carry a right-click menu.
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

        let editing = self.original_key.clone();
        let renaming = self
            .renaming_folder
            .as_ref()
            .map(|(folder, field)| (folder.clone(), field.clone()));
        let folder_count = self.macro_folders(cx).len();
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
                        tree = tree.child(self.render_folder_name_field(
                            index,
                            &field,
                            "この名前に変更します",
                            "名前の変更をやめます",
                            |this, window, cx| this.confirm_folder_rename(window, cx),
                            |this, _| this.renaming_folder = None,
                            window,
                            cx,
                        ));
                        continue;
                    }
                    tree = tree.child(self.render_folder_heading(
                        index,
                        folder,
                        collapsed,
                        count,
                        folder_count,
                        cx,
                    ));
                }
                MacroTreeRow::EmptyFolder { folder } => {
                    let filled = folder.clone();
                    let dropped = folder.clone();
                    tree = tree.child(
                        h_flex()
                            .id(("empty-folder", index))
                            .w_full()
                            .pl_5()
                            .py_0p5()
                            .rounded_sm()
                            .cursor_pointer()
                            .drag_over::<DraggedMacro>(|style, _, _, cx| {
                                style.bg(cx.theme().colors().drop_target_background)
                            })
                            .on_drop(cx.listener(
                                move |this, dragged: &DraggedMacro, window, cx| {
                                    this.move_macro_to_folder(
                                        &dragged.key,
                                        Some(dropped.clone()),
                                        window,
                                        cx,
                                    );
                                },
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.new_macro_in_folder(Some(filled.clone()), window, cx);
                            }))
                            .child(
                                Label::new(EMPTY_FOLDER_ROW)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    );
                }
                MacroTreeRow::Macro(terminal_macro) => {
                    let key = terminal_macro.key();
                    let name = terminal_macro.name.clone();
                    let selected = editing.as_ref() == Some(&key);
                    let detail = format!("{} ステップ", terminal_macro.steps.len());
                    tree = tree.child(
                        div()
                            .id(("saved-macro-row", index))
                            .w_full()
                            .on_drag(DraggedMacro { key }, {
                                let name = name.clone();
                                move |_, _, _, cx| {
                                    let label = SharedString::from(name.clone());
                                    cx.new(|_| DraggedMacroLabel(label))
                                }
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

    /// One folder heading. 未分類 gets the same row without the pencil and the
    /// menu, because there is no folder behind it to rename, reorder or delete.
    fn render_folder_heading(
        &self,
        index: usize,
        folder: Option<String>,
        collapsed: bool,
        count: usize,
        folder_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let heading = folder.clone().unwrap_or_else(|| UNFILED_FOLDER.to_string());
        let disclosure_folder = folder.clone();
        let label_folder = folder.clone();
        let row = h_flex()
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
                    this.move_macro_to_folder(&dragged.key, folder.clone(), window, cx);
                }
            }))
            .child(
                Disclosure::new(("macro-folder-toggle", index), !collapsed).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.toggle_folder_collapsed(disclosure_folder.clone(), cx);
                    },
                )),
            )
            .child(
                h_flex()
                    .id(("macro-folder-label", index))
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_folder_collapsed(label_folder.clone(), cx);
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
            .when_some(folder.clone(), |this, folder| {
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
            });

        let Some(name) = folder else {
            return row.into_any_element();
        };
        let position = self
            .store
            .read(cx)
            .file()
            .folder_index(&name)
            .unwrap_or(usize::MAX);
        let view = cx.weak_entity();
        right_click_menu(("macro-folder-menu", index))
            .trigger(move |_, _, _| row)
            .menu(move |window, cx| {
                let name = name.clone();
                let view = view.clone();
                ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.entry("新しいマクロ", None, {
                        let name = name.clone();
                        let view = view.clone();
                        move |window, cx| {
                            let name = name.clone();
                            view.update(cx, |this, cx| {
                                this.new_macro_in_folder(Some(name), window, cx);
                            })
                            .ok();
                        }
                    });
                    menu = menu.entry("名前を変更", None, {
                        let name = name.clone();
                        let view = view.clone();
                        move |window, cx| {
                            let name = name.clone();
                            view.update(cx, |this, cx| {
                                this.begin_folder_rename(name, window, cx);
                            })
                            .ok();
                        }
                    });
                    if position > 0 {
                        menu = menu.entry("上へ移動", None, {
                            let name = name.clone();
                            let view = view.clone();
                            move |_, cx| {
                                view.update(cx, |this, cx| this.move_folder(&name, true, cx))
                                    .ok();
                            }
                        });
                    }
                    if position + 1 < folder_count {
                        menu = menu.entry("下へ移動", None, {
                            let name = name.clone();
                            let view = view.clone();
                            move |_, cx| {
                                view.update(cx, |this, cx| this.move_folder(&name, false, cx))
                                    .ok();
                            }
                        });
                    }
                    menu.separator().entry("削除…", None, {
                        move |window, cx| {
                            let name = name.clone();
                            view.update(cx, |this, cx| {
                                this.request_folder_delete(name, window, cx);
                            })
                            .ok();
                        }
                    })
                })
            })
            .into_any_element()
    }

    /// The inline folder-name field, shared by 「名前を変更」 in the tree and
    /// 「新しいフォルダー」 in both the tree's toolbar and the form's dropdown.
    #[allow(clippy::too_many_arguments)]
    fn render_folder_name_field(
        &self,
        index: usize,
        field: &Entity<Editor>,
        confirm_tooltip: &'static str,
        cancel_tooltip: &'static str,
        confirm: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cancel: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
                IconButton::new(("confirm-folder-name", index), IconName::Check)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(confirm_tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        confirm(this, window, cx);
                    })),
            )
            .child(
                IconButton::new(("cancel-folder-name", index), IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(cancel_tooltip))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cancel(this, cx);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The form's folder chooser. A dropdown rather than a free-text field: a
    /// macro can only be filed under a folder that exists, and a new one is
    /// made from the last entry (ADR-F35).
    fn render_folder_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folders = self.known_folders(cx);
        let selected = self
            .form_folder
            .clone()
            .unwrap_or_else(|| NO_FOLDER.to_string());
        let view = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.entry(NO_FOLDER, None, {
                let view = view.clone();
                move |_, cx| {
                    view.update(cx, |this, cx| {
                        this.form_folder = None;
                        cx.notify();
                    })
                    .ok();
                }
            });
            for folder in &folders {
                let folder = folder.clone();
                let view = view.clone();
                menu = menu.entry(folder.clone(), None, move |_, cx| {
                    let folder = folder.clone();
                    view.update(cx, |this, cx| {
                        this.form_folder = Some(folder);
                        cx.notify();
                    })
                    .ok();
                });
            }
            menu.separator().entry("新しいフォルダー…", None, {
                move |window, cx| {
                    view.update(cx, |this, cx| this.begin_new_folder(true, window, cx))
                        .ok();
                }
            })
        });

        DropdownMenu::new("macro-folder-choices", selected, menu).style(DropdownStyle::Outlined)
    }

    /// One child list of a row — the body of a 繰り返し, a side of a 分岐, or the
    /// 「タイムアウト時」 steps of a 期待文字を待つ — indented under its heading.
    fn render_child_list(
        &self,
        path: &StepPath,
        list: StepList,
        heading: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.row(path) else {
            return div().into_any_element();
        };
        let element_id = row.element_id();
        let child_count = row.list(list).len();
        // Deeper levels stay in the row tree and are written back untouched;
        // only the form stops drawing them, because the indentation (and the
        // step numbers) stop being readable.
        if path.depth() >= MAX_EDITABLE_STEP_DEPTH {
            return v_flex()
                .w_full()
                .pl_4()
                .child(
                    Label::new(format!(
                        "{heading}: {child_count} ステップ (これより深い入れ子は JSON で編集してください)"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element();
        }

        let child_rows = (0..child_count)
            .map(|index| self.render_step(&path.child(list, index), window, cx))
            .collect::<Vec<_>>();
        let add_id = match list {
            StepList::Main => "add-main-step",
            StepList::Else => "add-else-step",
            StepList::Timeout => "add-timeout-step",
        };
        let parent = path.clone();
        v_flex()
            .w_full()
            .gap_1()
            .pl_4()
            .child(
                Label::new(heading)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(child_rows)
            .child(
                Button::new((add_id, element_id), "子ステップを追加")
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.add_child_step(&parent, list, StepKind::Send, window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_step(
        &self,
        path: &StepPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.row(path) else {
            return div().into_any_element();
        };
        let kind = row.kind;
        let element_id = row.element_id();
        let index = path.index();
        let sibling_count = match path.0.split_last() {
            Some((_, [])) => self.steps.len(),
            Some((last, parent)) => self
                .row(&StepPath(Rc::new(parent.to_vec())))
                .map(|parent_row| parent_row.list(last.0).len())
                .unwrap_or(0),
            None => 0,
        };

        let view = cx.weak_entity();
        let kind_menu = ContextMenu::build(window, cx, {
            let path = path.clone();
            move |mut menu, _, _| {
                for candidate in StepKind::ALL {
                    let view = view.clone();
                    let path = path.clone();
                    menu = menu.toggleable_entry(
                        candidate.label(),
                        candidate == kind,
                        IconPosition::Start,
                        None,
                        move |window, cx| {
                            view.update(cx, |this, cx| {
                                let Some(row) = this.row_mut(&path) else {
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
                                // The hint names the field, so it follows the
                                // kind the row was just switched to.
                                let placeholder = candidate.text_placeholder();
                                row.text.update(cx, |editor, cx| {
                                    editor.set_placeholder_text(placeholder, window, cx);
                                });
                                cx.notify();
                            })
                            .ok();
                        },
                    );
                }
                menu
            }
        });

        let scope_menu = (kind == StepKind::If).then(|| {
            let view = cx.weak_entity();
            let path = path.clone();
            let selected = row.scope;
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for candidate in [MatchScope::SinceLastSend, MatchScope::Screen] {
                    let view = view.clone();
                    let path = path.clone();
                    menu = menu.toggleable_entry(
                        candidate.label(),
                        candidate == selected,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if let Some(row) = this.row_mut(&path) {
                                    row.scope = candidate;
                                    cx.notify();
                                }
                            })
                            .ok();
                        },
                    );
                }
                menu
            })
        });

        let timeout_menu = (kind == StepKind::WaitFor).then(|| {
            let view = cx.weak_entity();
            let path = path.clone();
            let selected = row.continue_on_timeout;
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (candidate, label) in TIMEOUT_CHOICES {
                    let view = view.clone();
                    let path = path.clone();
                    menu = menu.toggleable_entry(
                        label,
                        candidate == selected,
                        IconPosition::Start,
                        None,
                        move |_, cx| {
                            view.update(cx, |this, cx| {
                                if let Some(row) = this.row_mut(&path) {
                                    row.continue_on_timeout = candidate;
                                    cx.notify();
                                }
                            })
                            .ok();
                        },
                    );
                }
                menu
            })
        });

        let repeat_children = (kind == StepKind::Repeat)
            .then(|| self.render_child_list(path, StepList::Main, "繰り返す内容", window, cx));
        let branch_children = (kind == StepKind::If).then(|| {
            [
                self.render_child_list(path, StepList::Main, "そのとき", window, cx),
                self.render_child_list(path, StepList::Else, "それ以外", window, cx),
            ]
        });
        let timeout_children = (kind == StepKind::WaitFor && row.continue_on_timeout)
            .then(|| self.render_child_list(path, StepList::Timeout, "タイムアウト時", window, cx));

        let move_up = path.clone();
        let move_down = path.clone();
        let delete = path.clone();
        let newline_path = path.clone();
        let regex_path = path.clone();
        let selector = path.debug_selector();
        v_flex()
            .w_full()
            .gap_1()
            .p_1p5()
            .rounded_sm()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .debug_selector(move || selector)
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
                            .child(
                                DropdownMenu::new(
                                    ("step-kind", element_id),
                                    kind.label(),
                                    kind_menu,
                                )
                                .style(DropdownStyle::Outlined),
                            ),
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
                                        let Some(siblings) = this.siblings_mut(&move_up) else {
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
                                        let Some(siblings) = this.siblings_mut(&move_down) else {
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
                                        let Some(siblings) = this.siblings_mut(&delete) else {
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
            .when(kind.has_text(), |this| {
                this.child(
                    input_frame_sized(
                        InputFrameSize::Compact,
                        row.text.focus_handle(cx).contains_focused(window, cx),
                        cx,
                    )
                    .child(row.text.clone()),
                )
            })
            .when(kind == StepKind::Send, |this| {
                this.child(
                    Checkbox::new(("step-newline", element_id), row.newline.into())
                        .label("末尾で Enter を送る")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                            let checked = *state == ToggleState::Selected;
                            if let Some(row) = this.row_mut(&newline_path)
                                && row.newline != checked
                            {
                                row.newline = checked;
                                cx.notify();
                            }
                        })),
                )
            })
            .when(kind == StepKind::Repeat, |this| {
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
            .when(kind.has_duration(), |this| {
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
                        ),
                )
            })
            .when(kind.has_pattern(), |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Checkbox::new(("step-regex", element_id), row.regex.into())
                                .label("正規表現")
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, state: &ToggleState, _, cx| {
                                    let checked = *state == ToggleState::Selected;
                                    if let Some(row) = this.row_mut(&regex_path)
                                        && row.regex != checked
                                    {
                                        row.regex = checked;
                                        cx.notify();
                                    }
                                })),
                        )
                        .when_some(scope_menu, |this, menu| {
                            this.child(
                                Label::new("対象")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                DropdownMenu::new(
                                    ("step-scope", element_id),
                                    row.scope.label(),
                                    menu,
                                )
                                .style(DropdownStyle::Outlined),
                            )
                        }),
                )
            })
            .when(kind == StepKind::WaitFor || kind == StepKind::If, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new("取り込む変数名")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            input_frame_sized(
                                InputFrameSize::Compact,
                                row.capture.focus_handle(cx).contains_focused(window, cx),
                                cx,
                            )
                            .w(rems(8.))
                            .child(row.capture.clone()),
                        )
                        .when(kind == StepKind::WaitFor, |this| {
                            this.child(
                                Label::new("タイムアウト時")
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .when_some(timeout_menu, |this, menu| {
                                this.child(
                                    DropdownMenu::new(
                                        ("step-timeout", element_id),
                                        timeout_choice_label(row.continue_on_timeout),
                                        menu,
                                    )
                                    .style(DropdownStyle::Outlined),
                                )
                            })
                        }),
                )
            })
            .children(timeout_children)
            .children(branch_children.into_iter().flatten())
            .children(repeat_children)
            .into_any_element()
    }
}

/// How deep the form unfolds a step tree. Rows below this stay in the macro and
/// are saved unchanged, but the indentation and the 「3-2-1-2-1」 numbering stop
/// being worth reading, so they are only editable in `terminal_sessions.json`.
const MAX_EDITABLE_STEP_DEPTH: usize = 5;

/// What 「タイムアウト時」 offers, `continue_on_timeout` first.
const TIMEOUT_CHOICES: [(bool, &str); 2] = [
    (false, "マクロを中止する"),
    (true, "次のステップを実行する"),
];

fn timeout_choice_label(continue_on_timeout: bool) -> &'static str {
    TIMEOUT_CHOICES
        .iter()
        .find(|(candidate, _)| *candidate == continue_on_timeout)
        .map(|(_, label)| *label)
        .unwrap_or_default()
}

impl Render for MacroEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steps = (0..self.steps.len())
            .map(|index| self.render_step(&StepPath::top(index), window, cx))
            .collect::<Vec<_>>();
        let tree = self.render_macro_tree(window, cx);
        let listed = self.listed_macros(cx).len();
        let total = self.store.read(cx).macros().len();
        let tree_header = if listed == total {
            format!("保存済みのマクロ ({total} 件)")
        } else {
            format!("保存済みのマクロ ({listed} / {total} 件)")
        };
        let form_header = match &self.original_key {
            Some((_, name)) => format!("『{name}』を編集"),
            None => "新規マクロ".to_string(),
        };
        let editing = self.original_key.clone();
        let pending_delete = self.pending_delete.clone();
        let pending_folder_delete = self.pending_folder_delete.clone();
        let pending_selection = self.pending_selection.clone();
        let folder_menu = self.render_folder_menu(window, cx).into_any_element();
        // The same inline field serves the tree's toolbar and the form's
        // dropdown; it is drawn wherever the request came from.
        let new_folder_field = self.new_folder.as_ref().map(|entry| {
            (
                entry.file_open_macro_in_it,
                self.render_folder_name_field(
                    usize::MAX,
                    &entry.field,
                    "この名前でフォルダーを作ります",
                    "フォルダーの作成をやめます",
                    |this, _, cx| this.confirm_new_folder(cx),
                    |this, _| this.new_folder = None,
                    window,
                    cx,
                ),
            )
        });
        let (new_folder_in_form, new_folder_in_tree) = match new_folder_field {
            Some((true, field)) => (Some(field), None),
            Some((false, field)) => (None, Some(field)),
            None => (None, None),
        };
        let field = |editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>| {
            input_frame(editor.focus_handle(cx).contains_focused(window, cx), cx)
                .child(editor.clone())
        };
        let list_filter_field = field(&self.list_filter, window, cx);
        let name_field = field(&self.name, window, cx);
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
                                                h_flex()
                                                    .gap_1()
                                                    .child(
                                                        Button::new("new-macro", "新規")
                                                            .label_size(LabelSize::Small)
                                                            .start_icon(Icon::new(IconName::Plus))
                                                            .tooltip(Tooltip::text(
                                                                "空のマクロを右側に開きます",
                                                            ))
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.select_macro(
                                                                        None, window, cx,
                                                                    );
                                                                },
                                                            )),
                                                    )
                                                    .child(
                                                        Button::new("new-folder", "フォルダー")
                                                            .label_size(LabelSize::Small)
                                                            .start_icon(Icon::new(IconName::Plus))
                                                            .tooltip(Tooltip::text(
                                                                "マクロをまとめるフォルダーを作ります",
                                                            ))
                                                            .on_click(cx.listener(
                                                                |this, _, window, cx| {
                                                                    this.begin_new_folder(
                                                                        false, window, cx,
                                                                    );
                                                                },
                                                            )),
                                                    ),
                                            ),
                                    )
                                    .child(list_filter_field)
                                    .children(new_folder_in_tree)
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
                                            .when_some(editing, |this, key| {
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
                                                                            key.clone(),
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
                                                    .gap_2()
                                                    .child(
                                                        Label::new("フォルダー")
                                                            .size(LabelSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .child(folder_menu),
                                            )
                                            .children(new_folder_in_form)
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
                                        let prompt = match edited
                                            .as_ref()
                                            .filter(|edited| !edited.name.is_empty())
                                        {
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
                                    .when_some(pending_folder_delete, |this, folder| {
                                        let moved = folder.clone();
                                        let removed = folder.clone();
                                        this.child(
                                            v_flex()
                                                .w_full()
                                                .gap_1()
                                                .child(
                                                    Label::new(format!(
                                                        "フォルダー『{folder}』を削除します。中のマクロをどうしますか？"
                                                    ))
                                                    .size(LabelSize::Small)
                                                    .color(Color::Warning),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new(
                                                                "folder-delete-keep",
                                                                "マクロを未分類へ移す",
                                                            )
                                                            .on_click(cx.listener(
                                                                move |this, _, window, cx| {
                                                                    this.delete_folder(
                                                                        &moved, true, window, cx,
                                                                    );
                                                                },
                                                            )),
                                                        )
                                                        .child(
                                                            Button::new(
                                                                "folder-delete-all",
                                                                "マクロごと削除",
                                                            )
                                                            .color(Color::Error)
                                                            .on_click(cx.listener(
                                                                move |this, _, window, cx| {
                                                                    this.delete_folder(
                                                                        &removed, false, window,
                                                                        cx,
                                                                    );
                                                                },
                                                            )),
                                                        )
                                                        .child(
                                                            Button::new(
                                                                "folder-delete-cancel",
                                                                "キャンセル",
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.pending_folder_delete =
                                                                        None;
                                                                    cx.notify();
                                                                },
                                                            )),
                                                        ),
                                                ),
                                        )
                                    })
                                    .when_some(pending_delete, |this, key| {
                                        let name = key.1;
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
    /// Which macro runs once the terminal is up, by folder and name: the same
    /// name may exist in several folders (ADR-F35).
    macro_key: Option<MacroKey>,
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
            macro_key: None,
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
            macro_name: self.macro_key.as_ref().map(|(_, name)| name.clone()),
            macro_folder: self
                .macro_key
                .as_ref()
                .and_then(|(folder, _)| folder.clone()),
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
        self.macro_key = connection
            .macro_name
            .clone()
            .map(|name| (connection.macro_folder.clone(), name));
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
        let terminal_macro = self
            .store
            .read(cx)
            .macro_for_connection(&connection)
            .cloned();
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
        let folders = self.store.read(cx).macro_folders().to_vec();
        let mut macros = self.store.read(cx).macros().to_vec();
        sort_macros(&mut macros, &folders);
        // 「フォルダー / 名前」, because the name on its own no longer says which
        // macro this is (ADR-F35).
        let selected_macro_label = match &self.macro_key {
            Some((Some(folder), name)) => format!("{folder} / {name}"),
            Some((None, name)) => name.clone(),
            None => "(なし)".to_string(),
        };
        let view = cx.weak_entity();
        let macro_menu = ContextMenu::build(window, cx, {
            move |mut menu, _, _| {
                menu = menu.entry("(なし)", None, {
                    let view = view.clone();
                    move |_, cx| {
                        view.update(cx, |this, cx| {
                            this.macro_key = None;
                            cx.notify();
                        })
                        .ok();
                    }
                });
                // Same folder grouping as the macro picker, so a long list stays
                // navigable here too.
                let mut current_folder: Option<&str> = None;
                for terminal_macro in &macros {
                    let folder = terminal_macro.folder_display();
                    if current_folder != Some(folder) {
                        current_folder = Some(folder);
                        if !folder.is_empty() {
                            menu = menu.header(folder.to_string());
                        }
                    }
                    let key = terminal_macro.key();
                    let name = terminal_macro.name.clone();
                    let view = view.clone();
                    menu = menu.entry(name, None, move |_, cx| {
                        let key = key.clone();
                        view.update(cx, |this, cx| {
                            this.macro_key = Some(key);
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
                    MacroPickerEntry::Folder { .. } => None,
                })
                .collect()
        })
    }

    /// Every row of the picker, folder headings rendered as 「名前 (件数)」.
    fn picker_rows(picker: &Entity<MacroPicker>, cx: &mut VisualTestContext) -> Vec<String> {
        picker.update(cx, |picker, cx| {
            let picker = picker.picker.read(cx);
            picker
                .delegate
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MacroPickerEntry::Folder {
                        folder,
                        count,
                        collapsed,
                    } => Some(format!(
                        "{}{}",
                        MacroPickerEntry::heading_label(folder.as_deref(), *count),
                        if *collapsed { " [畳]" } else { "" }
                    )),
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

    /// The layout ADR-F35 was written for: a folder per target device, with the
    /// same macro name in two of them.
    const TWO_DEVICE_MACROS: &str = r#"{
        "macro_folders": [
            { "name": "ルータ用" },
            { "name": "A-PC用" },
            { "name": "B-PC用" }
        ],
        "macros": [
            { "name": "show run", "folder": "ルータ用", "steps": [] },
            { "name": "show ver", "folder": "ルータ用", "steps": [] },
            { "name": "ls", "folder": "A-PC用", "description": "A", "steps": [] },
            { "name": "ls", "folder": "B-PC用", "description": "B", "steps": [] }
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

        // Folders come in the order the file lists them, each heading carries
        // its count, and 未分類 is last.
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "拠点/大阪 (1)",
                "再起動",
                "拠点/本社 (2)",
                "ログイン",
                "設定保存",
                "未分類 (1)",
                "メモ",
            ]
        );

        // The headings must not be selectable, and the first selectable row is
        // the one the picker starts on.
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                assert_eq!(picker.delegate.selected_index(), 1);
                assert!(!picker.delegate.can_select(0, window, cx));
                assert!(picker.delegate.can_select(1, window, cx));
                assert_eq!(picker.delegate.separators_after_indices(), vec![1, 4]);
            });
        });
    }

    #[gpui::test]
    async fn the_picker_shows_the_same_name_from_two_folders(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "ルータ用 (2)",
                "show run",
                "show ver",
                "A-PC用 (1)",
                "ls",
                "B-PC用 (1)",
                "ls",
            ]
        );

        // Both 「ls」 stay reachable, each under its own device's folder.
        set_picker_query(&picker, "ls", cx);
        assert_eq!(
            picker_names(&picker, cx),
            vec!["ls".to_string(), "ls".to_string()]
        );
    }

    #[gpui::test]
    async fn the_picker_draws_a_heading_clear_of_the_macro_row_above_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx).len(),
            7,
            "three headings, each with the macros under it"
        );

        // A macro row is two lines (name over 「n ステップ」) and a heading is
        // one, so a list that lays every row out at a single height draws
        // 「A-PC用 (1)」 on top of the 「1 ステップ」 line above it.
        const ROWS: [&str; 7] = [
            "macro-picker-row-0",
            "macro-picker-row-1",
            "macro-picker-row-2",
            "macro-picker-row-3",
            "macro-picker-row-4",
            "macro-picker-row-5",
            "macro-picker-row-6",
        ];
        let mut previous: Option<(&str, gpui::Bounds<gpui::Pixels>)> = None;
        for selector in ROWS {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} should have been drawn"));
            if let Some((above, above_bounds)) = previous {
                assert!(
                    above_bounds.bottom() <= bounds.top(),
                    "{above} ends at {} but {selector} starts at {}",
                    above_bounds.bottom(),
                    bounds.top()
                );
            }
            previous = Some((selector, bounds));
        }
    }

    #[gpui::test]
    async fn a_picker_row_puts_the_step_count_beside_the_name(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "ルータ用 (2)",
                "show run",
                "show ver",
                "A-PC用 (1)",
                "ls",
                "B-PC用 (1)",
                "ls",
            ]
        );

        // Row 1 is 「show run」 (no description), row 4 is 「ls」 (description
        // 「A」): both are one line, so the description must not add a second.
        let plain = cx
            .debug_bounds("macro-picker-row-1")
            .expect("the first macro row should have been drawn");
        let described = cx
            .debug_bounds("macro-picker-row-4")
            .expect("the described macro row should have been drawn");
        assert_eq!(
            plain.size.height, described.size.height,
            "a description belongs beside the name, not under it"
        );

        // 「n ステップ」 is right of the name and on the same line as it.
        let detail = cx
            .debug_bounds("macro-picker-detail-1")
            .expect("the step count should have been drawn");
        assert!(
            detail.left() >= plain.right(),
            "the step count starts at {} but the name runs to {}",
            detail.left(),
            plain.right()
        );
        assert!(
            detail.top() < plain.bottom() && plain.top() < detail.bottom(),
            "the step count ({detail:?}) is not on the name's line ({plain:?})"
        );
    }

    #[gpui::test]
    async fn a_collapsed_folder_hides_its_macros_until_the_query_asks_for_them(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();

        // Clicking a heading folds it, and that goes into the file.
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                MacroPickerDelegate::toggle_folder(
                    picker,
                    Some("ルータ用".to_string()),
                    false,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();

        assert_eq!(
            picker_rows(&picker, cx),
            vec!["ルータ用 (2) [畳]", "A-PC用 (1)", "ls", "B-PC用 (1)", "ls",]
        );
        assert!(
            saved_file(&fs)
                .await
                .macro_folders
                .iter()
                .any(|folder| folder.name == "ルータ用" && folder.collapsed),
            "the collapse state belongs in the file"
        );

        // A query must never be answered with a folded heading.
        set_picker_query(&picker, "show", cx);
        assert_eq!(
            picker_rows(&picker, cx),
            vec!["ルータ用 (2)", "show run", "show ver"]
        );
    }

    /// `count` unfiled macros, which is more than any picker can show at once.
    fn many_macros(count: usize) -> String {
        let macros = (0..count)
            .map(|index| format!(r#"{{ "name": "マクロ{index:02}", "steps": [] }}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{ \"macros\": [{macros}] }}")
    }

    #[gpui::test]
    async fn a_long_macro_list_scrolls_instead_of_growing_the_picker(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, &many_macros(80)).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(picker_names(&picker, cx).len(), 80);

        // The list is capped, so the far end of it is not laid out at all —
        // which is what makes the modal a fixed size rather than 80 rows tall.
        assert!(
            cx.debug_bounds("macro-picker-row-0").is_some(),
            "the top of the list is on screen"
        );
        assert!(
            cx.debug_bounds("macro-picker-row-79").is_none(),
            "an 80-row picker must not draw its last row without scrolling"
        );

        // Keyboard navigation has to bring the selected row into view.
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                picker.set_selected_index(79, None, true, window, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("macro-picker-row-79").is_some(),
            "selecting the last macro should scroll it into view"
        );
        assert!(
            cx.debug_bounds("macro-picker-row-0").is_none(),
            "and scroll the top of the list away"
        );
    }

    #[gpui::test]
    async fn the_setting_opens_the_picker_with_every_folder_folded(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        cx.update(|_, cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .terminal
                        .get_or_insert_default()
                        .macro_picker_folders_collapsed = Some(true);
                });
            });
        });
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx),
            vec!["ルータ用 (2) [畳]", "A-PC用 (1) [畳]", "B-PC用 (1) [畳]"],
            "nothing in the file says these are folded; the setting does"
        );
        assert!(
            saved_file(&fs)
                .await
                .macro_folders
                .iter()
                .all(|folder| !folder.collapsed),
            "the setting must not rewrite what the editor tree reads"
        );

        // A query still answers with every match, folded headings and all.
        set_picker_query(&picker, "ls", cx);
        assert_eq!(
            picker_rows(&picker, cx),
            vec!["A-PC用 (1)", "ls", "B-PC用 (1)", "ls"]
        );

        // Clicking a heading still opens it, for this picker only.
        set_picker_query(&picker, "", cx);
        picker.update_in(cx, |picker, window, cx| {
            picker.picker.update(cx, |picker, cx| {
                MacroPickerDelegate::toggle_folder(
                    picker,
                    Some("A-PC用".to_string()),
                    true,
                    window,
                    cx,
                );
            });
        });
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx),
            vec!["ルータ用 (2) [畳]", "A-PC用 (1)", "ls", "B-PC用 (1) [畳]"]
        );
        assert!(
            saved_file(&fs)
                .await
                .macro_folders
                .iter()
                .all(|folder| !folder.collapsed),
            "with the setting on the file has nothing to say, so it is left alone"
        );
    }

    #[gpui::test]
    async fn the_picker_can_fold_and_unfold_every_folder_at_once(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert!(picker.update(cx, |picker, cx| {
            !picker.picker.read(cx).delegate.every_folder_collapsed()
        }));

        let fold_all = |collapsed: bool, cx: &mut VisualTestContext| {
            picker.update_in(cx, |picker, window, cx| {
                picker.picker.update(cx, |picker, cx| {
                    MacroPickerDelegate::set_every_folder_collapsed(picker, collapsed, window, cx);
                });
            });
            cx.run_until_parked();
        };

        fold_all(true, cx);
        assert_eq!(
            picker_rows(&picker, cx),
            vec!["ルータ用 (2) [畳]", "A-PC用 (1) [畳]", "B-PC用 (1) [畳]"]
        );
        assert!(picker.update(cx, |picker, cx| {
            picker.picker.read(cx).delegate.every_folder_collapsed()
        }));
        assert!(
            saved_file(&fs)
                .await
                .macro_folders
                .iter()
                .all(|folder| !folder.collapsed),
            "folding everything is a way of looking at the list, not a change to it"
        );

        fold_all(false, cx);
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "ルータ用 (2)",
                "show run",
                "show ver",
                "A-PC用 (1)",
                "ls",
                "B-PC用 (1)",
                "ls",
            ]
        );
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
                    MacroPickerEntry::Folder { .. } => None,
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
                editor.read(cx).original_key,
                Some((None, "ログイン".to_string())),
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
    async fn every_level_of_a_nested_repeat_is_editable(cx: &mut TestAppContext) {
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
            assert_eq!(
                children[1].children.len(),
                1,
                "so do the steps of the repeat two levels down"
            );
        });

        editor.update_in(cx, |editor, window, cx| {
            // A step appended two levels down, which the old one-level form
            // could not reach at all.
            editor.add_child_step(
                &StepPath::top(0).child(StepList::Main, 1),
                StepList::Main,
                StepKind::Sleep,
                window,
                cx,
            );
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
                steps: vec![
                    sent,
                    MacroStep::Repeat {
                        count: 2,
                        steps: vec![MacroStep::Clear, MacroStep::Sleep { ms: 1_000 }],
                    },
                ],
            }]
        );
    }

    /// A branch whose own capture, two sides, nested capture and timeout list
    /// exercise every field the form gained, nested deeply enough that the
    /// tree has to be walked.
    fn branching_macro() -> TerminalMacro {
        TerminalMacro {
            name: "復旧".to_string(),
            steps: vec![
                MacroStep::Send {
                    text: "show ${target}".to_string(),
                    newline: true,
                },
                MacroStep::If {
                    pattern: "Invalid input".to_string(),
                    regex: false,
                    scope: MatchScope::Screen,
                    capture: Some("reason".to_string()),
                    then_steps: vec![MacroStep::Repeat {
                        count: 2,
                        steps: vec![MacroStep::WaitFor {
                            pattern: r"(?P<prompt>\S+)#".to_string(),
                            regex: true,
                            timeout_ms: 3_000,
                            capture: Some("banner".to_string()),
                            on_timeout: Some(vec![MacroStep::Clear]),
                        }],
                    }],
                    else_steps: vec![MacroStep::Send {
                        text: "write memory".to_string(),
                        newline: true,
                    }],
                },
            ],
            ..TerminalMacro::default()
        }
    }

    #[gpui::test]
    async fn a_branch_a_capture_and_a_timeout_list_survive_a_trip_through_the_form(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let edited = branching_macro();
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(edited.clone()), window, cx);
        });

        editor.read_with(cx, |editor, cx| {
            let branch = &editor.steps[1];
            assert_eq!(branch.kind, StepKind::If);
            assert_eq!(branch.scope, MatchScope::Screen);
            assert_eq!(branch.capture.read(cx).text(cx), "reason");
            assert_eq!(branch.children.len(), 1, "「そのとき」 keeps its repeat");
            assert_eq!(branch.else_children.len(), 1, "「それ以外」 keeps its send");
            let wait = &branch.children[0].children[0];
            assert_eq!(wait.capture.read(cx).text(cx), "banner");
            assert!(wait.continue_on_timeout);
            assert_eq!(wait.timeout_children.len(), 1);
        });

        editor.update_in(cx, |editor, _, cx| editor.save(cx));
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        assert_eq!(parse_sessions_file(&contents).macros, vec![edited]);
    }

    #[gpui::test]
    async fn a_wait_that_stops_the_macro_forgets_the_steps_it_would_have_run(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "待つ".to_string(),
                    steps: vec![MacroStep::WaitFor {
                        pattern: "router#".to_string(),
                        regex: false,
                        timeout_ms: 1_000,
                        capture: None,
                        on_timeout: Some(vec![MacroStep::Clear]),
                    }],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            // 「マクロを中止する」 is not the same as an empty list: the steps
            // stop being part of the macro.
            if let Some(row) = editor.row_mut(&StepPath::top(0)) {
                row.continue_on_timeout = false;
            }
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
            vec![MacroStep::WaitFor {
                pattern: "router#".to_string(),
                regex: false,
                timeout_ms: 1_000,
                capture: None,
                on_timeout: None,
            }]
        );
    }

    #[gpui::test]
    async fn the_form_draws_every_level_of_a_branch(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(branching_macro()), window, cx);
        });
        cx.run_until_parked();

        // 2 = the 分岐, 2-1 = the 繰り返し on its 「そのとき」 side, 2-1-1 = the
        // wait inside that, 2-1-1-t1 = the step its timeout runs, and 2-e1 =
        // the 「それ以外」 side, which shares its number with 2-1.
        for selector in [
            "macro-step-1",
            "macro-step-2",
            "macro-step-2-1",
            "macro-step-2-1-1",
            "macro-step-2-1-1-t1",
            "macro-step-2-e1",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} should have been drawn"
            );
        }

        let then_side = cx
            .debug_bounds("macro-step-2-1")
            .expect("the 「そのとき」 row should have been drawn");
        let else_side = cx
            .debug_bounds("macro-step-2-e1")
            .expect("the 「それ以外」 row should have been drawn");
        assert!(
            then_side.bottom() <= else_side.top(),
            "「そのとき」 ends at {} but 「それ以外」 starts at {}",
            then_side.bottom(),
            else_side.top()
        );
    }

    /// Seven levels of steps, threaded through a 「そのとき」, a
    /// 「タイムアウト時」 and an 「それ以外」 so that every kind of child list has
    /// to be walked. The last two levels are past
    /// [`MAX_EDITABLE_STEP_DEPTH`].
    fn deeply_nested_macro() -> TerminalMacro {
        TerminalMacro {
            name: "深い".to_string(),
            steps: vec![MacroStep::Repeat {
                count: 2,
                steps: vec![MacroStep::If {
                    pattern: "二段目".to_string(),
                    regex: false,
                    scope: MatchScope::SinceLastSend,
                    capture: None,
                    then_steps: vec![MacroStep::Repeat {
                        count: 3,
                        steps: vec![MacroStep::WaitFor {
                            pattern: "四段目".to_string(),
                            regex: false,
                            timeout_ms: 2_000,
                            capture: Some("四".to_string()),
                            on_timeout: Some(vec![MacroStep::If {
                                pattern: "五段目".to_string(),
                                regex: true,
                                scope: MatchScope::Screen,
                                capture: None,
                                then_steps: vec![MacroStep::Send {
                                    text: "六段目".to_string(),
                                    newline: false,
                                }],
                                else_steps: vec![MacroStep::Repeat {
                                    count: 4,
                                    steps: vec![MacroStep::Sleep { ms: 1_500 }],
                                }],
                            }]),
                        }],
                    }],
                    else_steps: Vec::new(),
                }],
            }],
            ..TerminalMacro::default()
        }
    }

    #[gpui::test]
    async fn steps_below_the_drawn_depth_are_folded_away_and_saved_unchanged(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let edited = deeply_nested_macro();
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(edited.clone()), window, cx);
        });
        cx.run_until_parked();

        for selector in [
            "macro-step-1",
            "macro-step-1-1",
            "macro-step-1-1-1",
            "macro-step-1-1-1-1",
            "macro-step-1-1-1-1-t1",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} is within the drawn depth"
            );
        }
        for selector in ["macro-step-1-1-1-1-t1-1", "macro-step-1-1-1-1-t1-e1"] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector} is past 深さ {MAX_EDITABLE_STEP_DEPTH} and is only editable as JSON"
            );
        }

        editor.update_in(cx, |editor, _, cx| editor.save(cx));
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        assert_eq!(
            parse_sessions_file(&contents).macros,
            vec![edited],
            "a step the form declined to draw still has to come back out of it"
        );
    }

    #[gpui::test]
    async fn moving_and_deleting_reach_the_side_list_the_row_sits_in(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let send = |text: &str| MacroStep::Send {
            text: text.to_string(),
            newline: true,
        };
        // 「そのとき」 is three steps long as well, so a reorder that reached the
        // wrong list would quietly succeed instead of running off the end.
        let edited = TerminalMacro {
            name: "整理".to_string(),
            steps: vec![MacroStep::If {
                pattern: "点検".to_string(),
                regex: false,
                scope: MatchScope::SinceLastSend,
                capture: None,
                then_steps: vec![
                    MacroStep::WaitFor {
                        pattern: "router#".to_string(),
                        regex: false,
                        timeout_ms: 1_000,
                        capture: None,
                        on_timeout: Some(vec![MacroStep::Clear, MacroStep::Sleep { ms: 500 }]),
                    },
                    MacroStep::Clear,
                    MacroStep::Sleep { ms: 2_000 },
                ],
                else_steps: vec![send("一"), send("二"), send("三")],
            }],
            ..TerminalMacro::default()
        };

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(edited), window, cx);
        });
        editor.update_in(cx, |editor, _, cx| {
            // 「ひとつ上へ移動します」 on the last row of 「それ以外」.
            let moved = StepPath::top(0).child(StepList::Else, 2);
            let siblings = editor
                .siblings_mut(&moved)
                .expect("the 「それ以外」 list is the row's own list");
            // Bound-checked the way the button is, so a list mixup shows up as
            // the wrong steps rather than as a panic.
            if moved.index() > 0 && moved.index() < siblings.len() {
                siblings.swap(moved.index() - 1, moved.index());
            }

            // 「このステップを削除します」 on the first row of the
            // 「タイムアウト時」 list of the wait that opens 「そのとき」.
            let deleted = StepPath::top(0)
                .child(StepList::Main, 0)
                .child(StepList::Timeout, 0);
            let siblings = editor
                .siblings_mut(&deleted)
                .expect("the 「タイムアウト時」 list is the row's own list");
            if deleted.index() < siblings.len() {
                siblings.remove(deleted.index());
            }

            editor.save(cx);
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        assert_eq!(
            parse_sessions_file(&contents).macros[0].steps,
            vec![MacroStep::If {
                pattern: "点検".to_string(),
                regex: false,
                scope: MatchScope::SinceLastSend,
                capture: None,
                then_steps: vec![
                    MacroStep::WaitFor {
                        pattern: "router#".to_string(),
                        regex: false,
                        timeout_ms: 1_000,
                        capture: None,
                        on_timeout: Some(vec![MacroStep::Sleep { ms: 500 }]),
                    },
                    MacroStep::Clear,
                    MacroStep::Sleep { ms: 2_000 },
                ],
                else_steps: vec![send("一"), send("三"), send("二")],
            }]
        );
    }

    #[gpui::test]
    async fn a_branch_without_a_pattern_names_the_step(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("分岐", window, cx);
            });
            editor.add_step(StepKind::Clear, window, cx);
            editor.add_step(StepKind::If, window, cx);
            editor.save(cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("ステップ 2: 判定する文字列を入力してください。")
            );
        });
    }

    #[gpui::test]
    async fn a_capture_name_that_would_break_expansion_names_the_step(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("取り込み", window, cx);
            });
            editor.add_step(StepKind::WaitFor, window, cx);
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("router#", window, cx);
            });
            editor.steps[0].capture.update(cx, |capture, cx| {
                capture.set_text("${host}", window, cx);
            });
            editor.save(cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("ステップ 1: 取り込む変数名に $ { } は使えません。")
            );
        });
    }

    #[gpui::test]
    async fn an_if_capture_name_that_would_break_expansion_names_the_step(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("分岐取り込み", window, cx);
            });
            editor.add_step(StepKind::If, window, cx);
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("Invalid input", window, cx);
            });
            editor.steps[0].capture.update(cx, |capture, cx| {
                capture.set_text("${host}", window, cx);
            });
            editor.save(cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("ステップ 1: 取り込む変数名に $ { } は使えません。")
            );
        });
    }

    /// The label a save error carries is built while walking the rows, not by
    /// [`StepPath::label`], so it needs its own hold on the spelling: a step
    /// two lists down has to be named the way a failing run would name it.
    #[gpui::test]
    async fn a_bad_step_under_a_branch_is_named_by_the_lists_it_sits_in(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "分岐".to_string(),
                    steps: vec![MacroStep::If {
                        pattern: "Invalid input".to_string(),
                        regex: false,
                        scope: MatchScope::Screen,
                        capture: None,
                        then_steps: Vec::new(),
                        else_steps: vec![MacroStep::WaitFor {
                            pattern: "router#".to_string(),
                            regex: false,
                            timeout_ms: 1_000,
                            capture: None,
                            on_timeout: Some(vec![MacroStep::WaitFor {
                                pattern: "rt01#".to_string(),
                                regex: false,
                                timeout_ms: 1_000,
                                capture: Some("${host}".to_string()),
                                on_timeout: None,
                            }]),
                        }],
                    }],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            editor.save(cx);
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("ステップ 1-e1-t1: 取り込む変数名に $ { } は使えません。")
            );
        });
    }

    /// [`StepPath::label`] has to spell a position exactly the way a running
    /// macro's error messages do
    /// ([`terminal::terminal_macro::MacroRunner::step_label`]), and
    /// [`StepPath::debug_selector`] has to agree with it too — a test that
    /// finds a row by one and reads a message that names the other must be
    /// looking at the same step.
    #[test]
    fn step_labels_spell_which_list_a_position_came_from() {
        let then_step = StepPath::top(1).child(StepList::Main, 0);
        let else_step = StepPath::top(1).child(StepList::Else, 0);
        let timeout_step = StepPath::top(0).child(StepList::Timeout, 0);

        assert_eq!(then_step.label(), "2-1");
        assert_eq!(else_step.label(), "2-e1");
        assert_eq!(timeout_step.label(), "1-t1");

        for path in [&then_step, &else_step, &timeout_step] {
            assert_eq!(
                path.debug_selector(),
                format!("macro-step-{}", path.label())
            );
        }
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
    async fn saving_files_the_macro_under_the_chosen_folder_and_keeps_the_old_entries(
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
            // The folder comes from the dropdown, which only ever offers a
            // folder that exists — so one is made first, the way
            // 「新しいフォルダー…」 does.
            editor.begin_new_folder(true, window, cx);
            let Some(entry) = editor.new_folder.as_ref() else {
                panic!("the name field should be open");
            };
            entry.field.update(cx, |field, cx| {
                field.set_text(" 拠点 / 本社 / ", window, cx);
            });
            editor.confirm_new_folder(cx);
        });
        // Creating the folder writes the file; letting that land keeps the save
        // below from racing it.
        cx.run_until_parked();

        editor.update_in(cx, |editor, window, cx| {
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
        assert_eq!(
            saved.macro_folders,
            vec![terminal::terminal_macro::MacroFolder::new("拠点/本社")],
            "the folder name is normalized when it is created"
        );
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

        // Unfiltered, the list follows the folder order, 未分類 last.
        assert_eq!(
            listed(&editor, cx),
            vec!["再起動", "ログイン", "設定保存", "メモ"]
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
                    MacroTreeRow::EmptyFolder { folder } => format!("{folder}/ (空)"),
                    MacroTreeRow::Macro(terminal_macro) => terminal_macro.name,
                })
                .collect::<Vec<_>>()
        })
    }

    fn stored_macro(
        editor: &Entity<MacroEditor>,
        folder: Option<&str>,
        name: &str,
        cx: &mut VisualTestContext,
    ) -> TerminalMacro {
        editor.update(cx, |editor, cx| {
            editor
                .store
                .read(cx)
                .macro_named(folder, name)
                .unwrap_or_else(|| panic!("『{name}』 should be in the store"))
                .clone()
        })
    }

    /// The macro folders in the order the file lists them, which is the order
    /// the tree and the picker follow.
    fn folder_names(
        store: &Entity<TerminalSessionStore>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        cx.update(|_, cx| {
            store
                .read(cx)
                .macro_folders()
                .iter()
                .map(|folder| folder.name.clone())
                .collect()
        })
    }

    async fn saved_file(fs: &Arc<FakeFs>) -> terminal::terminal_macro::TerminalSessionsFile {
        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        parse_sessions_file(&contents)
    }

    #[gpui::test]
    async fn the_tree_groups_by_folder_and_collapses_a_heading(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        // The folders a pre-ADR-F35 file only implied are back, in the order
        // its macros first mention them, and 未分類 comes last.
        assert_eq!(
            folder_names(&store, cx),
            vec!["拠点/大阪".to_string(), "拠点/本社".to_string()]
        );
        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "拠点/大阪/ (1)",
                "再起動",
                "拠点/本社/ (2)",
                "ログイン",
                "設定保存",
                "未分類/ (1)",
                "メモ",
            ]
        );

        // A collapsed heading keeps its count but hides what is under it, and
        // says so in the file.
        editor.update(cx, |editor, cx| {
            editor.toggle_folder_collapsed(Some("拠点/本社".to_string()), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "拠点/大阪/ (1)",
                "再起動",
                "拠点/本社/ (2)",
                "未分類/ (1)",
                "メモ",
            ]
        );
        assert!(
            saved_file(&fs)
                .await
                .macro_folders
                .iter()
                .any(|folder| folder.name == "拠点/本社" && folder.collapsed)
        );

        // 未分類 has no folder in the file to keep its state in, so it is only
        // folded for as long as the modal is open.
        editor.update(cx, |editor, cx| {
            editor.toggle_folder_collapsed(None, cx);
        });
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "未分類/ (1)",]
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
        let login = stored_macro(&editor, Some("拠点/本社"), "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            assert!(
                !editor.has_unsaved_edits(cx),
                "a freshly opened blank form has nothing to discard"
            );
            editor.select_macro(Some(login), window, cx);
        });

        editor.update(cx, |editor, cx| {
            assert_eq!(
                editor.original_key,
                Some((Some("拠点/本社".to_string()), "ログイン".to_string()))
            );
            assert_eq!(editor.name.read(cx).text(cx), "ログイン");
            assert_eq!(editor.form_folder.as_deref(), Some("拠点/本社"));
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
        let login = stored_macro(&editor, Some("拠点/本社"), "ログイン", cx);
        let saved = stored_macro(&editor, Some("拠点/本社"), "設定保存", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.add_step(StepKind::Clear, window, cx);
            assert!(editor.has_unsaved_edits(cx), "a new step is an edit");
            editor.select_macro(Some(saved.clone()), window, cx);
        });

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.original_key.as_ref().map(|(_, name)| name.as_str()),
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
            assert_eq!(
                editor.original_key.as_ref().map(|(_, name)| name.as_str()),
                Some("設定保存")
            );
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
        let login = stored_macro(&editor, Some("拠点/本社"), "ログイン", cx);
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
        let restart = stored_macro(&editor, Some("拠点/大阪"), "再起動", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(restart), window, cx);
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

        assert_eq!(
            saved.macro_folders,
            vec![
                terminal::terminal_macro::MacroFolder::new("全社/大阪"),
                terminal::terminal_macro::MacroFolder::new("全社/本社"),
            ],
            "the folders themselves are renamed, keeping their order"
        );

        editor.update(cx, |editor, cx| {
            assert!(editor.renaming_folder.is_none());
            assert_eq!(
                editor.form_folder.as_deref(),
                Some("全社/大阪"),
                "the open macro follows its folder"
            );
            assert_eq!(
                editor.original_key,
                Some((Some("全社/大阪".to_string()), "再起動".to_string()))
            );
            assert!(
                !editor.has_unsaved_edits(cx),
                "a rename from the tree is not an unsaved form edit"
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
        let memo = stored_macro(&editor, None, "メモ", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(memo), window, cx);
            editor.move_macro_to_folder(
                &(None, "メモ".to_string()),
                Some("拠点/本社".to_string()),
                window,
                cx,
            );
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
            assert_eq!(editor.form_folder.as_deref(), Some("拠点/本社"));
            assert!(!editor.has_unsaved_edits(cx));
        });

        assert_eq!(
            saved_file(&fs)
                .await
                .macro_named(Some("拠点/本社"), "メモ")
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("メモ")
        );

        // Dropping it back on 未分類 clears the folder again.
        editor.update_in(cx, |editor, window, cx| {
            editor.move_macro_to_folder(
                &(Some("拠点/本社".to_string()), "メモ".to_string()),
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.form_folder, None);
            assert!(editor.store.read(cx).macro_named(None, "メモ").is_some());
        });
    }

    #[gpui::test]
    async fn moving_a_macro_into_a_folder_that_has_that_name_numbers_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let listing = stored_macro(&editor, Some("A-PC用"), "ls", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(listing), window, cx);
            editor.move_macro_to_folder(
                &(Some("A-PC用".to_string()), "ls".to_string()),
                Some("B-PC用".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        // Neither 「ls」 may be lost, so the moved one takes a numbered name.
        let saved = saved_file(&fs).await;
        assert_eq!(saved.macros_in_folder(Some("A-PC用")).count(), 0);
        let mut moved = saved
            .macros_in_folder(Some("B-PC用"))
            .map(|terminal_macro| terminal_macro.name.as_str())
            .collect::<Vec<_>>();
        moved.sort();
        assert_eq!(moved, vec!["ls", "ls (2)"]);
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.name.read(cx).text(cx), "ls (2)");
            assert_eq!(
                editor.original_key,
                Some((Some("B-PC用".to_string()), "ls (2)".to_string()))
            );
            assert!(!editor.has_unsaved_edits(cx));
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
                    { "name": "ログイン のコピー", "folder": "拠点/本社", "steps": [] },
                    { "name": "ログイン のコピー 2", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, Some("拠点/本社"), "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.duplicate_macro(window, cx);
        });

        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_key, None, "a copy is a new macro");
            // 「ログイン のコピー 2」 is taken, but under 未分類, so it is free
            // here: names only have to be unique inside their folder.
            assert_eq!(editor.name.read(cx).text(cx), "ログイン のコピー 2");
            assert_eq!(editor.form_folder.as_deref(), Some("拠点/本社"));
            assert_eq!(editor.steps.len(), 1);
            assert!(
                editor.has_unsaved_edits(cx),
                "a copy is unsaved until 保存, so switching away must warn"
            );
        });

        editor.update(cx, |editor, cx| editor.save(cx));
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(
            saved
                .macros
                .iter()
                .map(|terminal_macro| (
                    terminal_macro.folder.as_deref(),
                    terminal_macro.name.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                (Some("拠点/本社"), "ログイン"),
                (Some("拠点/本社"), "ログイン のコピー"),
                (None, "ログイン のコピー 2"),
                (Some("拠点/本社"), "ログイン のコピー 2"),
            ]
        );
        assert_eq!(
            saved.macros[3].steps,
            vec![MacroStep::Send {
                text: "enable".to_string(),
                newline: true
            }],
            "the copy carries the steps it was made from"
        );
    }

    #[gpui::test]
    async fn a_folder_can_be_made_empty_and_survives_a_save(cx: &mut TestAppContext) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "メモ", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.begin_new_folder(false, window, cx);
            let Some(entry) = editor.new_folder.as_ref() else {
                panic!("the name field should be open");
            };
            entry.field.update(cx, |field, cx| {
                field.set_text(" ルータ用 / ", window, cx);
            });
            editor.confirm_new_folder(cx);
        });
        cx.run_until_parked();

        assert_eq!(folder_names(&store, cx), vec!["ルータ用".to_string()]);
        // An empty folder is a real row that invites something to be put in it.
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["ルータ用/ (0)", "ルータ用/ (空)", "未分類/ (1)", "メモ"]
        );
        assert_eq!(
            saved_file(&fs).await.macro_folders,
            vec![terminal::terminal_macro::MacroFolder::new("ルータ用")],
            "an empty folder is part of the file"
        );

        // The same name twice is refused rather than making a second folder.
        editor.update_in(cx, |editor, window, cx| {
            editor.begin_new_folder(false, window, cx);
            let Some(entry) = editor.new_folder.as_ref() else {
                panic!("the name field should be open");
            };
            entry.field.update(cx, |field, cx| {
                field.set_text("ルータ用", window, cx);
            });
            editor.confirm_new_folder(cx);
        });
        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("同じ名前のフォルダーがあります。")
            );
            assert!(editor.new_folder.is_some(), "the field stays open");
        });
        assert_eq!(folder_names(&store, cx), vec!["ルータ用".to_string()]);
    }

    #[gpui::test]
    async fn the_empty_folder_row_starts_a_macro_already_filed_in_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macro_folders": [{ "name": "ルータ用" }], "macros": [] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["ルータ用/ (0)", "ルータ用/ (空)"]
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.new_macro_in_folder(Some("ルータ用".to_string()), window, cx);
        });
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_key, None, "it is still a new macro");
            assert_eq!(editor.form_folder.as_deref(), Some("ルータ用"));
            assert!(
                !editor.has_unsaved_edits(cx),
                "a blank form in a folder is not work to be warned about"
            );
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("show run", window, cx);
            });
            editor.save(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            saved_file(&fs)
                .await
                .macro_named(Some("ルータ用"), "show run")
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("show run")
        );
    }

    #[gpui::test]
    async fn folders_can_be_reordered_and_the_tree_follows(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update(cx, |editor, cx| editor.move_folder("B-PC用", true, cx));
        cx.run_until_parked();

        assert_eq!(
            folder_names(&store, cx),
            vec![
                "ルータ用".to_string(),
                "B-PC用".to_string(),
                "A-PC用".to_string()
            ]
        );
        assert_eq!(
            tree_rows(&editor, cx),
            vec![
                "ルータ用/ (2)",
                "show run",
                "show ver",
                "B-PC用/ (1)",
                "ls",
                "A-PC用/ (1)",
                "ls",
            ]
        );

        // The ends of the list are where 上へ / 下へ stop.
        editor.update(cx, |editor, cx| editor.move_folder("ルータ用", true, cx));
        editor.update(cx, |editor, cx| editor.move_folder("A-PC用", false, cx));
        cx.run_until_parked();
        assert_eq!(
            folder_names(&store, cx),
            vec![
                "ルータ用".to_string(),
                "B-PC用".to_string(),
                "A-PC用".to_string()
            ]
        );

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_rows(&picker, cx),
            vec![
                "ルータ用 (2)",
                "show run",
                "show ver",
                "B-PC用 (1)",
                "ls",
                "A-PC用 (1)",
                "ls",
            ],
            "the picker shows the order the editor arranged"
        );
    }

    #[gpui::test]
    async fn deleting_a_folder_either_unfiles_its_macros_or_takes_them_with_it(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "connections": [
                    { "name": "r1", "host": "10.0.0.1", "macro": "ls", "macro_folder": "A-PC用" }
                ],
                "macro_folders": [{ "name": "A-PC用" }, { "name": "空" }],
                "macros": [{ "name": "ls", "folder": "A-PC用", "steps": [] }]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        // An empty folder goes without asking.
        editor.update_in(cx, |editor, window, cx| {
            editor.request_folder_delete("空".to_string(), window, cx);
        });
        cx.run_until_parked();
        editor.update(cx, |editor, _| {
            assert!(editor.pending_folder_delete.is_none());
        });
        assert_eq!(folder_names(&store, cx), vec!["A-PC用".to_string()]);

        // One that still holds macros has to be answered for first.
        let listing = stored_macro(&editor, Some("A-PC用"), "ls", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(listing), window, cx);
            editor.request_folder_delete("A-PC用".to_string(), window, cx);
        });
        editor.update(cx, |editor, _| {
            assert_eq!(editor.pending_folder_delete.as_deref(), Some("A-PC用"));
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.delete_folder("A-PC用", true, window, cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert!(saved.macro_folders.is_empty());
        assert!(saved.macro_named(None, "ls").is_some(), "the macro stays");
        assert_eq!(
            saved.connections[0].macro_folder, None,
            "the connection keeps running it, now by name alone"
        );
        assert_eq!(
            saved
                .macro_for_connection(&saved.connections[0])
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("ls")
        );
        editor.update(cx, |editor, _| {
            assert_eq!(editor.form_folder, None);
            assert_eq!(editor.original_key, Some((None, "ls".to_string())));
        });

        // The other answer takes the macros with the folder.
        editor.update_in(cx, |editor, window, cx| {
            editor.move_macro_to_folder(
                &(None, "ls".to_string()),
                Some("A-PC用".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            editor.delete_folder("A-PC用", false, window, cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert!(saved.macros.is_empty(), "the macros went with the folder");
        assert!(saved.macro_folders.is_empty());
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.original_key, None);
            assert_eq!(editor.name.read(cx).text(cx), "");
        });
    }

    #[gpui::test]
    async fn the_same_name_in_two_folders_is_saved_twice(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "macro_folders": [{ "name": "A-PC用" }, { "name": "B-PC用" }],
                "macros": [{ "name": "ls", "folder": "A-PC用", "steps": [] }]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("ls", window, cx);
            });
            editor.form_folder = Some("B-PC用".to_string());
            editor.save(cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(
            saved
                .macros
                .iter()
                .map(|terminal_macro| (
                    terminal_macro.folder.as_deref(),
                    terminal_macro.name.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![(Some("A-PC用"), "ls"), (Some("B-PC用"), "ls")],
            "a name is only taken inside its own folder"
        );
    }

    #[gpui::test]
    async fn saving_a_name_the_same_folder_already_has_is_refused(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, TWO_DEVICE_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("show run", window, cx);
            });
            editor.form_folder = Some("ルータ用".to_string());
            editor.save(cx);
        });
        cx.run_until_parked();

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("同じフォルダーに同名のマクロがあります。")
            );
        });
        assert_eq!(
            saved_file(&fs)
                .await
                .macros_in_folder(Some("ルータ用"))
                .count(),
            2,
            "nothing may be written over"
        );

        // Renaming a macro onto a name another folder holds is fine. The
        // refused form still holds the rejected edits, so the macro is loaded
        // outright rather than through the 破棄しますか prompt.
        let listing = stored_macro(&editor, Some("A-PC用"), "ls", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(listing), window, cx);
            editor.name.update(cx, |name, cx| {
                name.set_text("show run", window, cx);
            });
            editor.save(cx);
        });
        cx.run_until_parked();
        let saved = saved_file(&fs).await;
        assert!(saved.macro_named(Some("A-PC用"), "show run").is_some());
        assert!(saved.macro_named(Some("ルータ用"), "show run").is_some());
    }

    #[gpui::test]
    async fn renaming_a_macro_takes_the_connections_that_run_it_along(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "connections": [
                    { "name": "r1", "host": "10.0.0.1", "macro": "ls", "macro_folder": "A-PC用" }
                ],
                "macro_folders": [{ "name": "A-PC用" }, { "name": "B-PC用" }],
                "macros": [{ "name": "ls", "folder": "A-PC用", "steps": [] }]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let listing = stored_macro(&editor, Some("A-PC用"), "ls", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(listing), window, cx);
            editor.name.update(cx, |name, cx| {
                name.set_text("dir", window, cx);
            });
            editor.form_folder = Some("B-PC用".to_string());
            editor.save(cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(saved.macros.len(), 1, "the macro moved, it was not copied");
        assert_eq!(saved.connections[0].macro_name.as_deref(), Some("dir"));
        assert_eq!(saved.connections[0].macro_folder.as_deref(), Some("B-PC用"));
        assert_eq!(
            saved
                .macro_for_connection(&saved.connections[0])
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("dir")
        );
    }

    /// A connection written before ADR-F35: the macro is filed under a folder,
    /// but the connection names it without one, which is also what an
    /// explicitly 未分類 reference looks like on disk.
    const LEGACY_CONNECTION: &str = r#"{
        "connections": [
            { "name": "r1", "host": "10.0.0.1", "macro": "ログイン" }
        ],
        "macro_folders": [{ "name": "拠点/本社" }, { "name": "拠点/大阪" }],
        "macros": [{ "name": "ログイン", "folder": "拠点/本社", "steps": [] }]
    }"#;

    #[gpui::test]
    async fn a_connection_that_names_its_macro_without_a_folder_follows_a_rename(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, LEGACY_CONNECTION).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, Some("拠点/本社"), "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.name.update(cx, |name, cx| {
                name.set_text("ログイン2", window, cx);
            });
            editor.save(cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(
            (
                saved.connections[0].macro_name.as_deref(),
                saved.connections[0].macro_folder.as_deref()
            ),
            (Some("ログイン2"), Some("拠点/本社")),
            "the older reference is rewritten in full, not left behind"
        );
        assert_eq!(
            saved
                .macro_for_connection(&saved.connections[0])
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("ログイン2")
        );
    }

    #[gpui::test]
    async fn a_connection_that_names_its_macro_without_a_folder_follows_a_move(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, LEGACY_CONNECTION).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.move_macro_to_folder(
                &(Some("拠点/本社".to_string()), "ログイン".to_string()),
                Some("拠点/大阪".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(
            (
                saved.connections[0].macro_name.as_deref(),
                saved.connections[0].macro_folder.as_deref()
            ),
            (Some("ログイン"), Some("拠点/大阪"))
        );
        assert_eq!(
            saved
                .macro_for_connection(&saved.connections[0])
                .and_then(|terminal_macro| terminal_macro.folder.as_deref()),
            Some("拠点/大阪")
        );
    }

    #[gpui::test]
    async fn a_connection_that_names_its_macro_without_a_folder_survives_a_folder_delete(
        cx: &mut TestAppContext,
    ) {
        // The folder is only implied by the macro filed under it, the way a
        // file written before ADR-F35 has it.
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "connections": [
                    { "name": "r1", "host": "10.0.0.1", "macro": "ログイン" }
                ],
                "macros": [
                    { "name": "ログイン", "folder": "拠点/本社", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.delete_folder("拠点/本社", true, window, cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert!(saved.macro_named(None, "ログイン").is_some());
        assert_eq!(
            saved
                .macro_for_connection(&saved.connections[0])
                .map(|terminal_macro| terminal_macro.name.as_str()),
            Some("ログイン")
        );

        // The other answer takes the macro with the folder, so the reference
        // goes as well — leaving it would put the folder back on the next load.
        editor.update_in(cx, |editor, window, cx| {
            editor.move_macro_to_folder(
                &(None, "ログイン".to_string()),
                Some("拠点/本社".to_string()),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            editor.delete_folder("拠点/本社", false, window, cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert!(saved.macros.is_empty());
        assert_eq!(saved.connections[0].macro_name, None);
        assert_eq!(saved.connections[0].macro_folder, None);
        assert!(saved.macro_folders.is_empty());
    }

    #[gpui::test]
    async fn deleting_a_macro_clears_the_connections_that_ran_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "connections": [
                    { "name": "r1", "host": "10.0.0.1", "macro": "ls", "macro_folder": "A-PC用" },
                    { "name": "r2", "host": "10.0.0.2", "macro": "ls" }
                ],
                "macro_folders": [{ "name": "A-PC用" }, { "name": "B-PC用" }],
                "macros": [
                    { "name": "ls", "folder": "A-PC用", "steps": [] },
                    { "name": "ls", "folder": "B-PC用", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.request_delete((Some("A-PC用".to_string()), "ls".to_string()), cx);
            editor.confirm_delete(window, cx);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs).await;
        assert_eq!(saved.macros.len(), 1, "only 「B-PC用/ls」 is left");
        // Both references named the macro that went away — the second one by
        // name alone — and neither may be left to run 「B-PC用/ls」 instead.
        for connection in &saved.connections {
            assert_eq!(connection.macro_name, None, "{}", connection.name);
            assert_eq!(connection.macro_folder, None, "{}", connection.name);
        }
    }

    #[gpui::test]
    async fn renaming_a_folder_onto_a_name_a_sub_folder_would_collide_with_is_refused(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "macro_folders": [{ "name": "拠点/本社" }, { "name": "全社/本社" }],
                "macros": [
                    { "name": "ログイン", "folder": "拠点/本社", "steps": [] },
                    { "name": "ログイン", "folder": "全社/本社", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        // 「拠点」 → 「全社」 also turns 「拠点/本社」 into 「全社/本社」, which is
        // already taken: two folders by that name, and two macros sharing one
        // key.
        editor.update_in(cx, |editor, window, cx| {
            editor.begin_folder_rename("拠点".to_string(), window, cx);
            let Some((_, field)) = editor.renaming_folder.clone() else {
                panic!("the rename should be in progress");
            };
            field.update(cx, |field, cx| {
                field.set_text("全社", window, cx);
            });
            editor.confirm_folder_rename(window, cx);
        });
        cx.run_until_parked();

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("同じ名前のフォルダーがあります。")
            );
            assert!(
                editor.renaming_folder.is_some(),
                "the field stays open so the name can be fixed"
            );
        });
        assert_eq!(
            folder_names(&store, cx),
            vec!["拠点/本社".to_string(), "全社/本社".to_string()],
            "nothing may be renamed when any of the results would collide"
        );
    }

    #[gpui::test]
    async fn the_tree_ignores_a_folded_heading_while_the_filter_is_on(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update(cx, |editor, cx| {
            editor.toggle_folder_collapsed(Some("拠点/本社".to_string()), cx);
            editor.toggle_folder_collapsed(None, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "未分類/ (1)"]
        );

        // A heading that says 1 件 must have that one row under it, folded or
        // not — the same rule the runner picker follows.
        let filter = |editor: &Entity<MacroEditor>, text: &str, cx: &mut VisualTestContext| {
            editor.update_in(cx, |editor, window, cx| {
                editor.list_filter.update(cx, |list_filter, cx| {
                    list_filter.set_text(text, window, cx);
                });
            });
        };
        filter(&editor, "ログイン", cx);
        assert_eq!(tree_rows(&editor, cx), vec!["拠点/本社/ (1)", "ログイン"]);
        editor.update(cx, |editor, cx| {
            assert!(
                editor.macro_tree(cx).iter().all(|row| !matches!(
                    row,
                    MacroTreeRow::Folder {
                        collapsed: true,
                        ..
                    }
                )),
                "a heading drawn folded while filtering would lie about its rows"
            );
        });

        // 未分類 keeps its collapse in the modal rather than the file, and has
        // to follow the same rule.
        filter(&editor, "メモ", cx);
        assert_eq!(tree_rows(&editor, cx), vec!["未分類/ (1)", "メモ"]);

        // Clearing the filter puts both back the way the user folded them.
        filter(&editor, "", cx);
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "未分類/ (1)"]
        );
    }

    #[gpui::test]
    async fn a_folder_rename_carries_the_open_delete_prompt_with_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, FOLDERED_MACROS).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.request_delete((Some("拠点/本社".to_string()), "ログイン".to_string()), cx);
            editor.begin_folder_rename("拠点".to_string(), window, cx);
            let Some((_, field)) = editor.renaming_folder.clone() else {
                panic!("the rename should be in progress");
            };
            field.update(cx, |field, cx| {
                field.set_text("全社", window, cx);
            });
            editor.confirm_folder_rename(window, cx);
        });
        cx.run_until_parked();

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.pending_delete,
                Some((Some("全社/本社".to_string()), "ログイン".to_string())),
                "confirming the prompt must still find the macro it named"
            );
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.confirm_delete(window, cx);
        });
        cx.run_until_parked();
        assert!(
            saved_file(&fs)
                .await
                .macro_named(Some("全社/本社"), "ログイン")
                .is_none(),
            "the macro the prompt named is the one that goes"
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

    /// ADR-F53: タブをどこに開く経路でも、接続先では同じ包みを通る。組み立ては
    /// 1 つしかないので、エージェントパネルのタブ経路 (`claude_code_launch_task`)
    /// だけがログインシェルを通らない、ということが起きない。
    #[test]
    fn the_task_is_wrapped_for_the_host_whichever_tab_it_opens_in() {
        let launch = ClaudeCodeCommand::Launch {
            effort: Some("high".to_string()),
        };
        let here = claude_code_spawn(&launch, None, false);
        assert_eq!(here.command.as_deref(), Some(CLAUDE_CODE_PROGRAM));
        assert_eq!(here.args, ["--effort", "high"]);

        let on_host = claude_code_spawn(&launch, None, true);
        assert_eq!(on_host.command.as_deref(), Some(REMOTE_SHELL));
        assert_eq!(
            on_host.args,
            [
                "-l",
                "-c",
                REMOTE_LAUNCH_SCRIPT,
                REMOTE_SCRIPT_NAME,
                "claude",
                "--effort",
                "high",
            ]
        );
        // 端末に見せるのは包む前の論理的なコマンド行。
        assert_eq!(on_host.command_label, here.command_label);
        assert_eq!(on_host.command_label, "claude --effort high");
        // インストールの導線も同じ包みを通る。
        let install = claude_code_spawn(&ClaudeCodeCommand::Install, None, true);
        assert_eq!(install.command.as_deref(), Some(REMOTE_SHELL));
        assert_eq!(install.args.get(4).map(String::as_str), Some(NPM_PROGRAM));
    }

    /// リモートでは引数をクォートせずに渡せる形 (`sh -l -c … $0 program args…`) で
    /// なければならない。ログインシェルを通すのも `~/.local/bin` を足すのもここ。
    #[test]
    fn the_remote_invocation_passes_arguments_as_positional_parameters() {
        let (program, args) = remote_invocation(
            REMOTE_LAUNCH_SCRIPT,
            CLAUDE_CODE_PROGRAM,
            &["--effort".to_string(), "high; touch /tmp/pwned".to_string()],
        );
        assert_eq!(program, REMOTE_SHELL);
        assert_eq!(
            args,
            vec![
                "-l",
                "-c",
                REMOTE_LAUNCH_SCRIPT,
                REMOTE_SCRIPT_NAME,
                "claude",
                "--effort",
                "high; touch /tmp/pwned",
            ]
        );
        // スクリプト側は位置引数しか見ないので、引数の中身がシェル構文として解釈される
        // 余地が無い。
        assert!(REMOTE_LAUNCH_SCRIPT.contains("exec \"$@\""));
        assert!(REMOTE_LAUNCH_SCRIPT.contains("$HOME/.local/bin"));
        // 探すときは `$0` がスクリプト名なので、プログラムは `$1`。
        let (_, lookup_args) = remote_invocation(REMOTE_LOOKUP_SCRIPT, CLAUDE_CODE_PROGRAM, &[]);
        assert_eq!(lookup_args.last().map(String::as_str), Some("claude"));
        assert_eq!(
            lookup_args.get(3).map(String::as_str),
            Some(REMOTE_SCRIPT_NAME)
        );
        assert!(REMOTE_LOOKUP_SCRIPT.contains("command -v -- \"$1\""));
    }

    /// 接続先が繋がったまま黙り込んでも、起動の前置きがそこで止まらない (ADR-F53)。
    /// 確かめられなかったのは「無い」ではないので、案内文を出さずに起動へ進む。
    #[gpui::test]
    async fn a_host_that_never_answers_does_not_hold_up_the_launch(cx: &mut TestAppContext) {
        let lookup = ClaudeCodeLookup {
            lookup: Some(ProgramLookup::NeverAnswers),
            program: CLAUDE_CODE_PROGRAM,
            on_remote_host: true,
        };
        let executor = cx.executor();
        let answered = Arc::new(AtomicBool::new(false));
        let message = cx.background_executor.spawn({
            let executor = executor.clone();
            let answered = answered.clone();
            async move {
                let message = lookup.missing_message(&executor).await;
                answered.store(true, std::sync::atomic::Ordering::SeqCst);
                message
            }
        });
        // ControlMaster が無い Windows だけ 30 秒。実装の選択を呼ばずにここで書くので、
        // 短いほうの値に潰れれば「期限の手前」の確認が落ちる。
        let expected = if cfg!(windows) {
            REMOTE_LOOKUP_TIMEOUT_WITHOUT_CONTROL_MASTER
        } else {
            REMOTE_LOOKUP_TIMEOUT
        };
        executor.advance_clock(expected - Duration::from_secs(1));
        assert!(
            !answered.load(std::sync::atomic::Ordering::SeqCst),
            "期限の手前ではまだ答えを待っている"
        );
        executor.advance_clock(Duration::from_secs(2));
        assert_eq!(
            message.await,
            None,
            "確かめられなかったときは止めずに起動へ進む"
        );
        assert!(REMOTE_LOOKUP_TIMEOUT_WITHOUT_CONTROL_MASTER > REMOTE_LOOKUP_TIMEOUT);
    }

    /// 期限を被せても、答えが返るときの答えは変わらない。上の「黙り込んだ」試験だけ
    /// では、常に起動へ進む実装でも通ってしまう。
    #[gpui::test]
    async fn a_host_that_answers_in_time_still_stops_the_launch(cx: &mut TestAppContext) {
        let lookup = ClaudeCodeLookup {
            // 実在しないプログラム名なので `find` は「無い」と答える。
            lookup: Some(ProgramLookup::OnThisMachine(
                "zed-jp-enhance-no-such-program".to_string(),
            )),
            program: CLAUDE_CODE_PROGRAM,
            on_remote_host: true,
        };
        let executor = cx.executor();
        let message = cx
            .background_executor
            .spawn({
                let executor = executor.clone();
                async move { lookup.missing_message(&executor).await }
            })
            .await;
        assert_eq!(
            message,
            Some(claude_code_missing_program_message(
                CLAUDE_CODE_PROGRAM,
                true
            )),
            "無いと分かったときは案内文を出して止める"
        );
    }

    /// 接続先に CLI が無いときは、こちらからインストールせず公式手順を案内する。
    #[test]
    fn the_missing_cli_message_names_where_to_install_it() {
        let on_host = claude_code_missing_program_message(CLAUDE_CODE_PROGRAM, true);
        assert!(on_host.contains("接続先のターミナル"), "{on_host}");
        assert!(
            on_host.contains("https://claude.ai/install.sh"),
            "{on_host}"
        );
        assert!(on_host.contains("~/.local/bin/claude"), "{on_host}");
        let here = claude_code_missing_program_message(CLAUDE_CODE_PROGRAM, false);
        assert!(!here.contains("接続先"), "{here}");
        let npm_on_host = claude_code_missing_program_message(NPM_PROGRAM, true);
        assert!(npm_on_host.contains("npm"), "{npm_on_host}");
        assert!(npm_on_host.contains("接続先"), "{npm_on_host}");
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
