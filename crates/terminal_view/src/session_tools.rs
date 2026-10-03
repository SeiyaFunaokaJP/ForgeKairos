//! TeraTerm-style session tooling: quick connect, the macro picker and the
//! macro editor.
//!
//! The data these modals read and write lives in `terminal_sessions.json`
//! (see [`terminal::terminal_macro`]); this module is only the UI over it.

use std::{
    cell::{Cell, RefCell},
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
    App, AsyncWindowContext, BackgroundExecutor, DismissEvent, DragMoveEvent, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, FutureExt as _, PromptLevel, Render, ScrollHandle,
    Subscription, Task, WeakEntity, Window, actions, canvas,
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
        check_pattern_regex, normalize_folder, normalize_tags, referenced_variables,
        references_followed_by_repetition,
    },
    terminal_settings::TerminalSettings,
};
use ui::{
    Checkbox, ContextMenu, ContextMenuEntry, Disclosure, DocumentationSide, DropdownMenu,
    DropdownStyle, HighlightedLabel, InputFrameSize, ListItem, ListItemSpacing, Modal, ModalFooter,
    ModalHeader, PopoverMenu, Section, ToggleState, Tooltip, WithScrollbar, input_frame,
    input_frame_sized, prelude::*, right_click_menu,
};
use util::ResultExt as _;
use util::command::Stdio;
use workspace::{
    ItemId, ModalView, Pane, Panel as _, SerializableItem, Workspace, WorkspaceId,
    item::{Item, ItemBufferKind, ItemEvent, SaveOptions},
};
use zed_actions::terminal::InstallClaudeCode;

use crate::{TerminalPanel, TerminalView};

actions!(
    terminal,
    [
        /// Chooses a saved macro and runs it in the active terminal.
        RunMacro,
        /// Edits the list of macros.
        EditMacros,
        /// Opens a terminal connected to an SSH or Telnet host.
        QuickConnect,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<MacroEditor>(cx);
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
            open_macro_editor(workspace, None, window, cx);
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
            spawn_claude_code_command(workspace, ClaudeCodeCommand::Install, window, cx, None);
        });
    })
    .detach();
}

/// ADR-F56: Zed's IDE tools (the `zed` MCP server) for a CLI started on this
/// machine. `agent_ui` builds it, since only it can reach `ide_server`.
pub struct ClaudeCodeIdeTools {
    /// `--mcp-config <JSON>` and `--allowedTools <…>`, holding no secret.
    pub args: Vec<String>,
    /// What the JSON reads the token from (`${…}`, which the CLI expands).
    pub env: Vec<(String, String)>,
    /// Held until the CLI's terminal finishes: dropping it ends the IDE
    /// server's registration of this CLI and clears what it drew.
    pub keep_alive: Option<Box<dyn std::any::Any>>,
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
    ide_tools: Option<ClaudeCodeIdeTools>,
) {
    spawn_claude_code_command(
        workspace,
        ClaudeCodeCommand::Launch { effort },
        window,
        cx,
        ide_tools,
    );
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
    ide_tools: Option<&ClaudeCodeIdeTools>,
) -> SpawnInTerminal {
    claude_code_task(
        &ClaudeCodeCommand::Launch { effort },
        workspace,
        cx,
        ide_tools,
    )
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
/// ここで組み立てるコマンドは**人間がターミナルに打つのと同じ実行経路**で、
/// 許可リストへのホスト追加はしない (ADR-F18 / ADR-F19)。ただしこの PC で
/// サンドボックスの外に起動する Claude Code には、ACP のエージェントを外で起動
/// するときと同じく egress のプロキシを指す環境変数を渡す (ADR-F49 決定 19、
/// 8c): 通信は、プロキシの設定を読むプログラムにだけ許可リストで絞られる
/// (協力方式。強制ではない)。接続先では、この PC のループバックのポートは別の
/// ものを指すので渡さない (統制なし)。
fn claude_code_task(
    command: &ClaudeCodeCommand,
    workspace: &Workspace,
    cx: &App,
    ide_tools: Option<&ClaudeCodeIdeTools>,
) -> SpawnInTerminal {
    let project = workspace.project().read(cx);
    claude_code_spawn(
        command,
        crate::first_project_directory(workspace, cx),
        project.remote_client().is_some(),
        http_client::apply_child_process_proxy_env,
        ide_tools,
    )
}

/// ADR-F56: the IDE tools' arguments for the local shell the launch goes
/// through. [`prepare_task_for_spawn`](crate::terminal_panel::prepare_task_for_spawn)
/// pastes every argument into the shell's command line as it is, so the JSON
/// (and the `${…}` in it, which the CLI and not the shell must expand) is
/// quoted here for that shell. `None` when an argument cannot be quoted for it.
fn ide_tool_arguments_for_shell(ide_tools: &ClaudeCodeIdeTools) -> Option<Vec<String>> {
    let shell_kind = task::Shell::System.shell_kind(cfg!(windows));
    ide_tools
        .args
        .iter()
        .map(|argument| {
            shell_kind
                .try_quote(argument)
                .map(|quoted| quoted.into_owned())
        })
        .collect()
}

/// [`claude_code_task`] の中身。どこのタブに開く経路も同じものを通すので、
/// 接続先の包み ([`remote_invocation`]) がどれか一方だけに掛かることがない
/// (ADR-F53)。
///
/// ADR-F56: `ide_tools` はこの PC で起動するときだけ付ける。接続先 (SSH) のホストの CLI
/// からは、この PC の loopback で待ち受ける Zed には届かない。
fn claude_code_spawn(
    command: &ClaudeCodeCommand,
    cwd: Option<PathBuf>,
    on_remote_host: bool,
    apply_proxy_env: fn(&mut HashMap<String, String>) -> bool,
    ide_tools: Option<&ClaudeCodeIdeTools>,
) -> SpawnInTerminal {
    let mut env = HashMap::default();
    let (program, args, task_id, label) = match command {
        ClaudeCodeCommand::Install => (
            NPM_PROGRAM.to_string(),
            vec![
                "install".to_string(),
                "-g".to_string(),
                CLAUDE_CODE_PACKAGE.to_string(),
            ],
            "terminal::install-claude-code",
            i18n::t!("Install Claude Code").to_string(),
        ),
        ClaudeCodeCommand::Launch { effort } => {
            let mut args = Vec::new();
            if let Some(ide_tools) = ide_tools.filter(|_| !on_remote_host) {
                match ide_tool_arguments_for_shell(ide_tools) {
                    Some(ide_arguments) => {
                        args.extend(ide_arguments);
                        env.extend(ide_tools.env.iter().cloned());
                    }
                    None => log::warn!(
                        "the IDE tools' arguments cannot be quoted for this shell; \
                         starting Claude Code without them"
                    ),
                }
            }
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
                i18n::t!("Claude Code (Outside the Sandbox)").to_string(),
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
    if matches!(command, ClaudeCodeCommand::Launch { .. }) && !on_remote_host {
        apply_proxy_env(&mut env);
    }

    SpawnInTerminal {
        id: TaskId(task_id.to_string()),
        full_label: label.clone(),
        label,
        command: Some(program),
        args,
        command_label,
        cwd,
        env,
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
    ide_tools: Option<ClaudeCodeIdeTools>,
) {
    let lookup = claude_code_lookup(claude_code_program(&command), workspace, cx);
    let spawn_task = claude_code_task(&command, workspace, cx, ide_tools.as_ref());
    let keep_alive = ide_tools.and_then(|ide_tools| ide_tools.keep_alive);

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
                    workspace.show_error(
                        i18n::t!("The terminal panel is unavailable.").to_string(),
                        cx,
                    )
                })
                .ok();
            return;
        };

        match spawn.await {
            Ok(terminal) => {
                // ADR-F56: the IDE server keeps this CLI's registration (and what it
                // drew) until the CLI exits.
                if let Some(keep_alive) = keep_alive
                    && let Ok(completion) =
                        terminal.read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                {
                    completion.await;
                    drop(keep_alive);
                }
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error(
                            i18n::t!("Couldn't run the command in the terminal: {error}"),
                            cx,
                        )
                    })
                    .ok();
            }
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
        (NPM_PROGRAM, false) => i18n::t!(
            "npm was not found on PATH. Installing Claude Code requires Node.js. \
             Install Node.js from https://nodejs.org/ and try again."
        )
        .to_string(),
        (NPM_PROGRAM, true) => i18n::t!(
            "npm was not found on the remote host. \
             Install Node.js in a terminal on the remote host and try again."
        )
        .to_string(),
        (_, false) => i18n::t!(
            "claude was not found on PATH. Run “terminal: install claude code” from the command palette first."
        )
        .to_string(),
        // ADR-F49 決定 6: 接続先へこちらから勝手にインストールはしない。公式手順を案内する。
        (_, true) => i18n::t!(
            "claude was not found on the remote host. \
             Install it by running this command in a terminal on the remote host:\n\
             curl -fsSL https://claude.ai/install.sh | bash\n\
             It is installed to `~/.local/bin/claude` and can be launched from then on."
        )
        .to_string(),
    }
}

fn workspace_fs(workspace: &Workspace) -> Option<Arc<dyn Fs>> {
    Some(workspace.app_state().fs.clone())
}

/// Shows the macro editor as a tab of the center pane. A macro that a tab
/// already has open brings that tab forward rather than opening a second form
/// over the same macro; a macro without a name (「新規作成」) always gets a
/// form of its own; `None` brings forward whichever editor tab is open, since
/// each one lists every saved macro.
fn open_macro_editor(
    workspace: &mut Workspace,
    edited: Option<TerminalMacro>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Option<Entity<MacroEditor>> {
    let mut open_editors = open_macro_editors(workspace, cx).into_iter();
    let existing = match &edited {
        None => open_editors.next(),
        Some(edited) if !edited.name.is_empty() => {
            let key = edited.key();
            open_editors.find(|(_, editor)| editor.read(cx).original_key.as_ref() == Some(&key))
        }
        Some(_) => None,
    };
    if let Some((pane, editor)) = existing {
        if !workspace.panes().contains(&pane) {
            workspace.reveal_panel::<TerminalPanel>(window, cx);
        }
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(&editor) {
                pane.activate_item(index, true, true, window, cx);
            }
        });
        return Some(editor);
    }
    let fs = workspace_fs(workspace)?;
    let store = TerminalSessionStore::global(fs, cx);
    let editor = cx.new(|cx| MacroEditor::new(store, edited, window, cx));
    workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    Some(editor)
}

/// Every macro editor tab with the pane holding it, the center's first. A tab
/// can be dragged into the terminal panel, whose panes the workspace's own
/// item lookups do not see.
fn open_macro_editors(workspace: &Workspace, cx: &App) -> Vec<(Entity<Pane>, Entity<MacroEditor>)> {
    let terminal_panes = workspace
        .panel::<TerminalPanel>(cx)
        .map(|panel| {
            panel
                .read(cx)
                .panes()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    workspace
        .panes()
        .iter()
        .cloned()
        .chain(terminal_panes)
        .flat_map(|pane| {
            pane.read(cx)
                .items_of_type::<MacroEditor>()
                .map(|editor| (pane.clone(), editor))
                .collect::<Vec<_>>()
        })
        .collect()
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
        return Err(i18n::t!(
            "There is no terminal to run the macro in. Open a terminal."
        ));
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
        format!("{} ({count})", folder.unwrap_or(unfiled_folder()))
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
        let workspace = self.workspace.clone();
        // Closing the picker goes through `ModalLayer::hide_modal`, which reads
        // the outgoing modal's focus handle — and `MacroPicker` forwards that to
        // this very picker. Doing it inline would read the picker while this
        // handler still has it leased, which panics with a double lease (on
        // Windows the window stops repainting first, so it reads as a freeze).
        // Deferring lets the effect cycle return the picker first.
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.hide_modal(window, cx);
                    open_macro_editor(workspace, edited, window, cx);
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
            self.error =
                Some(i18n::t!("Enter the repeat count as a whole number, 0 or more.").into());
            cx.notify();
            return;
        };
        let Some(repeat_interval_ms) = parse_seconds(&self.interval.read(cx).text(cx)) else {
            self.error = Some(i18n::t!("Enter the interval in seconds, 0 or more.").into());
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
                            .headline(i18n::t!("Run Repeatedly"))
                            .description(i18n::t!("Runs the macro “{name}” repeatedly.")),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new(i18n::t!("Repeat Count"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(count_field)
                                        .child(
                                            Label::new(i18n::t!("(0 repeats until stopped)"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new(i18n::t!("Interval (seconds)"))
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
                                    Button::new("cancel", i18n::t!("Cancel"))
                                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                                )
                                .child(Button::new("run", i18n::t!("Run")).on_click(cx.listener(
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
        i18n::t!("Filter by folder, name, or tag…").into()
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
            (IconName::ChevronUpDown, i18n::t!("Expand All"))
        } else {
            (IconName::ChevronDownUp, i18n::t!("Collapse All"))
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
            Some(i18n::t!("No saved macros").into())
        } else {
            Some(i18n::t!("Loading…").into())
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
                    Button::new("run-macro-repeatedly", i18n::t!("Run Repeatedly…"))
                        .tooltip(Tooltip::text(i18n::t!(
                            "Runs this macro with a repeat count and interval you choose."
                        )))
                        .on_click(cx.listener(|picker, _, window, cx| {
                            let selected = picker.delegate.selected_macro().cloned();
                            if let Some(terminal_macro) = selected {
                                picker.delegate.open_run_options(terminal_macro, window, cx);
                            }
                        })),
                )
                .child(
                    Button::new("edit-macro", i18n::t!("Edit")).on_click(cx.listener(
                        |picker, _, window, cx| {
                            let edited = picker.delegate.selected_macro().cloned();
                            if edited.is_some() {
                                picker.delegate.open_editor(edited, window, cx);
                            }
                        },
                    )),
                )
                .child(
                    Button::new("new-macro", i18n::t!("Create New")).on_click(cx.listener(
                        |picker, _, window, cx| {
                            picker
                                .delegate
                                .open_editor(Some(TerminalMacro::default()), window, cx);
                        },
                    )),
                )
                .into_any_element(),
        )
    }
}

/// The muted right-hand side of a picker row: how many steps the macro has, and
/// how often it repeats when that is not once.
fn macro_row_detail(terminal_macro: &TerminalMacro) -> String {
    let steps = i18n::tn!(terminal_macro.steps.len(), "{count} step", "{count} steps");
    match terminal_macro.iteration_count() {
        Some(1) => steps,
        Some(times) => i18n::t!("{steps} / ×{times}"),
        None => i18n::t!("{steps} / until stopped"),
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
fn loading_error() -> &'static str {
    i18n::t!("The settings file hasn't finished loading. Wait a moment and try again.")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepKind {
    Send,
    WaitFor,
    Sleep,
    Clear,
    Repeat,
    If,
    SetVariable,
}

impl StepKind {
    fn label(&self) -> &'static str {
        match self {
            StepKind::Send => i18n::tc!("terminal macro step", "Send"),
            StepKind::WaitFor => i18n::tc!("terminal macro step", "Wait for Text"),
            StepKind::Sleep => i18n::tc!("terminal macro step", "Sleep"),
            StepKind::Clear => i18n::tc!("terminal macro step", "Clear Screen"),
            StepKind::Repeat => i18n::tc!("terminal macro step", "Repeat"),
            StepKind::If => i18n::tc!("terminal macro step", "Branch"),
            StepKind::SetVariable => i18n::tc!("terminal macro step", "Set Variable"),
        }
    }

    /// The tooltip of the kind's 「追加」 button.
    fn description(&self) -> String {
        match self {
            StepKind::Send => i18n::t!("Sends text to the terminal").to_string(),
            StepKind::WaitFor => {
                i18n::t!("Waits until the given text appears in the output").to_string()
            }
            StepKind::Sleep => i18n::t!("Waits for the given number of seconds").to_string(),
            StepKind::Clear => i18n::t!("Clears the screen and the scrollback").to_string(),
            StepKind::Repeat => {
                i18n::t!("Repeats the steps inside it the given number of times").to_string()
            }
            StepKind::If => i18n::t!(
                "Chooses the steps to run by whether the output or a variable contains some text"
            )
            .to_string(),
            StepKind::SetVariable => i18n::t!(
                "Sets a variable. Wherever a later step says ${{name}}, it becomes the value"
            ),
        }
    }

    /// What a card of this kind shows beside its label, and what the 「+」
    /// menu shows beside the kind.
    fn icon(&self) -> IconName {
        match self {
            StepKind::Send => IconName::Send,
            StepKind::WaitFor => IconName::CountdownTimer,
            StepKind::Sleep => IconName::Clock,
            StepKind::Clear => IconName::Eraser,
            StepKind::Repeat => IconName::RotateCw,
            StepKind::If => IconName::GitBranch,
            StepKind::SetVariable => IconName::Hash,
        }
    }

    /// The stripe down the left edge of a card of this kind, so the flow
    /// reads by kind before any label does.
    fn stripe_color(&self) -> Color {
        match self {
            StepKind::Send => Color::Accent,
            StepKind::WaitFor | StepKind::Sleep => Color::Info,
            StepKind::If => Color::Warning,
            StepKind::Repeat => Color::Created,
            StepKind::SetVariable => Color::Modified,
            StepKind::Clear => Color::Muted,
        }
    }

    /// Whether the row shows the text field (期待文字 / 送信する文字列 /
    /// 判定する文字列 / 入れる値).
    fn has_text(&self) -> bool {
        matches!(
            self,
            StepKind::Send | StepKind::WaitFor | StepKind::If | StepKind::SetVariable
        )
    }

    /// Whether the row shows the seconds field.
    fn has_duration(&self) -> bool {
        matches!(self, StepKind::WaitFor | StepKind::Sleep)
    }

    /// Whether the row matches against the output, and so offers 正規表現.
    fn has_pattern(&self) -> bool {
        matches!(self, StepKind::WaitFor | StepKind::If)
    }

    /// Whether the row's `capture` field names a variable the row gives a
    /// value to.
    fn names_a_variable(&self) -> bool {
        matches!(
            self,
            StepKind::WaitFor | StepKind::If | StepKind::SetVariable
        )
    }

    /// The grey hint of the row's text field. It names the field and says that
    /// variables may be spliced in.
    fn text_placeholder(&self) -> String {
        match self {
            StepKind::Send => i18n::t!("Text to send (${{variable}} allowed)"),
            StepKind::WaitFor => i18n::t!("Expected text (${{variable}} allowed)"),
            StepKind::If => i18n::t!("Text to match (${{variable}} allowed)"),
            StepKind::SetVariable => i18n::t!("Value to set (${{variable}} allowed too)"),
            StepKind::Sleep | StepKind::Clear | StepKind::Repeat => String::new(),
        }
    }

    /// The grey hint of the row's `capture` field, which a wait or a branch may
    /// leave empty but a 変数に設定 cannot.
    fn capture_placeholder(&self) -> &'static str {
        match self {
            StepKind::SetVariable => variable_name_placeholder(),
            _ => capture_placeholder(),
        }
    }

    const ALL: [StepKind; 7] = [
        StepKind::Send,
        StepKind::WaitFor,
        StepKind::Sleep,
        StepKind::Clear,
        StepKind::Repeat,
        StepKind::If,
        StepKind::SetVariable,
    ];
}

/// The 対象 dropdown of a 分岐 row. [`MatchScope::Variable`] carries the
/// variable's name, which the form keeps in a field of its own, so the dropdown
/// needs a choice without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeChoice {
    SinceLastSend,
    Screen,
    Variable,
}

impl ScopeChoice {
    const ALL: [ScopeChoice; 3] = [
        ScopeChoice::SinceLastSend,
        ScopeChoice::Screen,
        ScopeChoice::Variable,
    ];

    fn of(scope: &MatchScope) -> Self {
        match scope {
            MatchScope::SinceLastSend => ScopeChoice::SinceLastSend,
            MatchScope::Screen => ScopeChoice::Screen,
            MatchScope::Variable(_) => ScopeChoice::Variable,
        }
    }

    fn scope(&self, variable: String) -> MatchScope {
        match self {
            ScopeChoice::SinceLastSend => MatchScope::SinceLastSend,
            ScopeChoice::Screen => MatchScope::Screen,
            ScopeChoice::Variable => MatchScope::Variable(variable),
        }
    }

    fn label(&self) -> &'static str {
        self.scope(String::new()).label()
    }
}

/// Reads a variable-name field. `${host}` is taken to mean `host`, because that
/// is how the name is spelled everywhere else in the form; a name that would
/// still break `${…}` expansion is refused.
fn parse_variable_name(value: &str) -> Result<Option<String>, ()> {
    let trimmed = value.trim();
    let name = trimmed
        .strip_prefix("${")
        .and_then(|name| name.strip_suffix('}'))
        .unwrap_or(trimmed)
        .trim();
    if name.contains(['$', '{', '}']) {
        return Err(());
    }
    Ok((!name.is_empty()).then(|| name.to_string()))
}

/// The 「分岐と変数の使い方」 panel, one paragraph per entry.
fn macro_help() -> [String; 5] {
    [
        i18n::t!(
            "Branch: runs the “Then” steps if the “Target” (output since last send / whole screen / variable) \
             contains the “Text to match”, and the “Else” steps if not. It checks right away without waiting, \
             so to check a response, put a “Wait for Text” step before it."
        )
        .to_string(),
        i18n::t!(
            "Setting variables: a “Set Variable” step, or the “Capture Into” field of “Wait for Text” and \
             “Branch” (it gets the line found, or the matched part for a regular expression). \
             A named group (?<name>…) of a regular expression puts the part it matched into the variable of that name."
        )
        .to_string(),
        i18n::t!(
            "Using variables: write ${{name}} in the text to send, the expected text, the text to match, \
             or the value to set, and it is replaced by the value when the macro runs. \
             To send ${{ as text, write $${{."
        ),
        i18n::t!(
            "Branching on a value: set the branch's “Target” to “Variable” to check the value of that variable. \
             A variable that has no value yet takes “Else”. Named groups of a regular expression \
             can pull part of a value out into another variable."
        )
        .to_string(),
        i18n::t!(
            "Example: Send “show version” → Wait for Text “Version (?<ver>[0-9.]+)” (regular expression) → \
             Branch (Target: variable ver, text to match “^15\\.”, regular expression) → Then: Send “…”"
        )
        .to_string(),
    ]
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

    /// The list the row sits in and its index there.
    fn lane_position(&self) -> Option<(Option<(StepPath, StepList)>, usize)> {
        let ((list, index), parent) = self.0.split_last()?;
        if parent.is_empty() {
            return Some((None, *index));
        }
        Some((Some((StepPath(Rc::new(parent.to_vec())), *list)), *index))
    }

    /// Whether `other` is this row or a row somewhere under it.
    fn contains(&self, other: &StepPath) -> bool {
        other.0.starts_with(&self.0)
    }
}

/// Where a step goes in: position `index` of the list `parent` names, `None`
/// being the macro's own steps. `index` may be the list's length, which is the
/// 「+」 at its end.
#[derive(Clone, PartialEq, Eq, Debug)]
struct InsertionPoint {
    parent: Option<(StepPath, StepList)>,
    index: usize,
}

impl InsertionPoint {
    fn path(&self) -> StepPath {
        lane_step_path(&self.parent, self.index)
    }

    /// Names the drawn 「+」 for [`gpui::VisualTestContext::debug_bounds`]:
    /// `macro-insert-top-0` before the macro's first step, `macro-insert-2-else-1`
    /// between the first and second step of the 「それ以外」 side of step 2.
    fn debug_selector(&self) -> String {
        format!("macro-insert-{}-{}", lane_key(&self.parent), self.index)
    }

    /// Whether moving the row at `from` here would change anything the
    /// macro does. A row cannot go inside itself, and dropping it right before
    /// or right after itself leaves it where it is.
    fn accepts(&self, from: &StepPath) -> bool {
        if let Some((parent, _)) = &self.parent
            && from.contains(parent)
        {
            return false;
        }
        match from.lane_position() {
            Some((lane, index)) => {
                !(lane == self.parent && (self.index == index || self.index == index + 1))
            }
            None => false,
        }
    }
}

fn lane_step_path(parent: &Option<(StepPath, StepList)>, index: usize) -> StepPath {
    match parent {
        Some((path, list)) => path.child(*list, index),
        None => StepPath::top(index),
    }
}

/// A position-based name for a list, for selectors and element ids: `top` for
/// the macro's own steps, `2-else` for the 「それ以外」 side of step 2.
fn lane_key(parent: &Option<(StepPath, StepList)>) -> String {
    match parent {
        Some((path, list)) => {
            let list = match list {
                StepList::Main => "main",
                StepList::Else => "else",
                StepList::Timeout => "timeout",
            };
            format!("{}-{list}", path.label())
        }
        None => "top".to_string(),
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
    /// The variable the row gives a value to: 取り込む変数名 of a 期待文字を待つ
    /// or a 分岐 row (empty means "capture nothing"), 変数名 of a 変数に設定
    /// row.
    capture: Entity<Editor>,
    regex: bool,
    newline: bool,
    /// What a 分岐 row decides on.
    scope: ScopeChoice,
    /// The variable a 分岐 row decides on when `scope` is
    /// [`ScopeChoice::Variable`].
    scope_variable: Entity<Editor>,
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
    /// A step this build cannot read ([`MacroStep::Unknown`]), kept so that
    /// saving writes it back as it was. Such a row is drawn as a placeholder
    /// with no fields; its `kind` is [`StepKind::Clear`] only so that every
    /// helper reading the fields treats it as a step without any.
    unknown: Option<MacroStep>,
    /// Whether the card shows its fields or only its header. Not saved: it is
    /// how the form is looked at, not part of the macro.
    expanded: bool,
    /// The last pattern text compiled for the row, so drawing the form does
    /// not compile every regular expression in it on every frame.
    pattern_check: RefCell<Option<PatternCheck>>,
    #[cfg(test)]
    pattern_compilations: std::cell::Cell<usize>,
}

/// What compiling a row's pattern text (as [`check_pattern_regex`] does) came
/// to.
struct PatternCheck {
    text: String,
    /// The pattern's named groups, or why it does not compile.
    outcome: Result<Vec<String>, String>,
}

/// The part of a `regex` error worth a line under the field: the crate's
/// message quotes the pattern with a caret under the fault over several lines
/// and ends with `error: <what is wrong>`.
fn regex_error_summary(error: &regex::Error) -> String {
    let message = error.to_string();
    message
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .map(str::to_string)
        .unwrap_or(message)
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
            MacroStep::Clear {} => StepKind::Clear,
            MacroStep::Repeat { .. } => StepKind::Repeat,
            MacroStep::If { .. } => StepKind::If,
            MacroStep::SetVariable { .. } => StepKind::SetVariable,
            MacroStep::Unknown(_) => StepKind::Clear,
        };
        let unknown = matches!(step, MacroStep::Unknown(_)).then(|| step.clone());
        let text_value = match step {
            MacroStep::Send { text, .. } | MacroStep::SetVariable { value: text, .. } => {
                text.clone()
            }
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
        let (scope, scope_variable_value) = match step {
            MacroStep::If {
                scope: MatchScope::Variable(name),
                ..
            } => (ScopeChoice::Variable, name.clone()),
            MacroStep::If { scope, .. } => (ScopeChoice::of(scope), String::new()),
            _ => (ScopeChoice::of(&MatchScope::default()), String::new()),
        };
        let capture_value = match step {
            MacroStep::WaitFor { capture, .. } | MacroStep::If { capture, .. } => {
                capture.clone().unwrap_or_default()
            }
            MacroStep::SetVariable { name, .. } => name.clone(),
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

        let single_line = |value: String, placeholder: &str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_text(value, window, cx);
                if !placeholder.is_empty() {
                    editor.set_placeholder_text(placeholder, window, cx);
                }
                editor
            })
        };
        let text = single_line(text_value, &kind.text_placeholder(), window, cx);
        let number = single_line(number_value, "", window, cx);
        let count = single_line(count_value, "", window, cx);
        let capture = single_line(capture_value, kind.capture_placeholder(), window, cx);
        let scope_variable = single_line(
            scope_variable_value,
            variable_name_placeholder(),
            window,
            cx,
        );

        Self {
            kind,
            text,
            number,
            count,
            capture,
            regex,
            newline,
            scope,
            scope_variable,
            continue_on_timeout,
            children,
            else_children,
            timeout_children,
            unknown,
            expanded: true,
            pattern_check: RefCell::new(None),
            #[cfg(test)]
            pattern_compilations: std::cell::Cell::new(0),
        }
    }

    /// The named groups of the row's pattern text, or why it does not compile
    /// as a regular expression. Compiled again only when the text changed
    /// since the last call.
    fn compiled_pattern(&self, cx: &App) -> Result<Vec<String>, String> {
        let text = self.text.read(cx).text(cx);
        let mut cache = self.pattern_check.borrow_mut();
        if let Some(check) = cache.as_ref()
            && check.text == text
        {
            return check.outcome.clone();
        }
        #[cfg(test)]
        self.pattern_compilations
            .set(self.pattern_compilations.get() + 1);
        let outcome = check_pattern_regex(&text)
            .map(|regex| {
                regex
                    .capture_names()
                    .flatten()
                    .map(str::to_string)
                    .collect()
            })
            .map_err(|error| regex_error_summary(&error));
        *cache = Some(PatternCheck {
            text,
            outcome: outcome.clone(),
        });
        outcome
    }

    /// Why the row's regular expression does not compile, for a regex row
    /// with something typed in.
    fn pattern_error(&self, cx: &App) -> Option<String> {
        if !self.kind.has_pattern() || !self.regex || self.text.read(cx).text(cx).trim().is_empty()
        {
            return None;
        }
        self.compiled_pattern(cx).err()
    }

    /// References in the row's regular expression that a repetition follows
    /// directly (`${p}+`).
    fn repeated_references(&self, cx: &App) -> Vec<String> {
        if !self.kind.has_pattern() || !self.regex {
            return Vec::new();
        }
        references_followed_by_repetition(&self.text.read(cx).text(cx))
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

    /// The child lists the row draws and saves. A row switched to another kind
    /// keeps its children in case it is switched back, but they are not part
    /// of the macro meanwhile.
    fn live_lists(&self) -> &'static [StepList] {
        match self.kind {
            StepKind::Repeat => &[StepList::Main],
            StepKind::If => &[StepList::Main, StepList::Else],
            StepKind::WaitFor if self.continue_on_timeout => &[StepList::Timeout],
            _ => &[],
        }
    }

    fn to_step(&self, label: &str, cx: &App) -> Result<MacroStep, String> {
        if let Some(step) = &self.unknown {
            return Ok(step.clone());
        }
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
        let capture = |cx: &App| -> Result<Option<String>, String> {
            parse_variable_name(&self.capture.read(cx).text(cx)).map_err(|()| {
                i18n::t!("Step {label}: the variable to capture into can't contain $, {{ or }}.")
            })
        };
        // A `${name}` in the pattern is only known at run time, so the check
        // compiles the pattern with each reference standing for a literal.
        let check_pattern = |text: &str| {
            if self.regex && check_pattern_regex(text).is_err() {
                Err(i18n::t!("Step {label}: invalid regular expression."))
            } else {
                Ok(())
            }
        };
        match self.kind {
            StepKind::Send => Ok(MacroStep::Send {
                text,
                newline: self.newline,
            }),
            StepKind::WaitFor => {
                if text.trim().is_empty() {
                    return Err(i18n::t!("Step {label}: enter the expected text."));
                }
                check_pattern(&text)?;
                Ok(MacroStep::WaitFor {
                    pattern: text,
                    regex: self.regex,
                    timeout_ms: parse_seconds(&self.number.read(cx).text(cx))
                        .ok_or_else(|| i18n::t!("Step {label}: invalid number of seconds."))?,
                    capture: capture(cx)?,
                    on_timeout: self
                        .continue_on_timeout
                        .then(|| steps(&self.timeout_children, StepList::Timeout, cx))
                        .transpose()?,
                })
            }
            StepKind::Sleep => Ok(MacroStep::Sleep {
                ms: parse_seconds(&self.number.read(cx).text(cx))
                    .ok_or_else(|| i18n::t!("Step {label}: invalid number of seconds."))?,
            }),
            StepKind::Clear => Ok(MacroStep::Clear {}),
            StepKind::Repeat => {
                let count = self
                    .count
                    .read(cx)
                    .text(cx)
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| {
                        i18n::t!(
                            "Step {label}: enter the repeat count as a whole number, 0 or more."
                        )
                    })?;
                Ok(MacroStep::Repeat {
                    count,
                    steps: steps(&self.children, StepList::Main, cx)?,
                })
            }
            StepKind::If => {
                if text.trim().is_empty() {
                    return Err(i18n::t!("Step {label}: enter the text to match."));
                }
                check_pattern(&text)?;
                let variable = match self.scope {
                    ScopeChoice::Variable => {
                        parse_variable_name(&self.scope_variable.read(cx).text(cx))
                            .map_err(|()| {
                                i18n::t!(
                                    "Step {label}: the variable to check can't contain $, {{ or }}."
                                )
                            })?
                            .ok_or_else(|| i18n::t!("Step {label}: enter the variable to check."))?
                    }
                    ScopeChoice::SinceLastSend | ScopeChoice::Screen => String::new(),
                };
                Ok(MacroStep::If {
                    pattern: text,
                    regex: self.regex,
                    scope: self.scope.scope(variable),
                    capture: capture(cx)?,
                    then_steps: steps(&self.children, StepList::Main, cx)?,
                    else_steps: steps(&self.else_children, StepList::Else, cx)?,
                })
            }
            StepKind::SetVariable => {
                let name = parse_variable_name(&self.capture.read(cx).text(cx))
                    .map_err(|()| {
                        i18n::t!("Step {label}: the variable name can't contain $, {{ or }}.")
                    })?
                    .ok_or_else(|| i18n::t!("Step {label}: enter a variable name."))?;
                Ok(MacroStep::SetVariable { name, value: text })
            }
        }
    }

    /// One sentence on what the row will do with the values it holds now, for
    /// the kinds whose behaviour the fields alone do not spell out.
    fn describe(&self, cx: &App) -> Option<String> {
        let text = self.text.read(cx).text(cx);
        let quoted = |fallback: &str| {
            if text.trim().is_empty() {
                i18n::t!("“{fallback}”")
            } else {
                i18n::t!("“{text}”")
            }
        };
        let variable =
            |field: &Entity<Editor>| parse_variable_name(&field.read(cx).text(cx)).ok().flatten();
        let captured = |sentence: &mut String, on_match: bool| {
            if let Some(name) = variable(&self.capture) {
                sentence.push_str(&match (on_match, self.regex) {
                    (false, false) => i18n::t!(" Puts the line found into ${{{name}}}."),
                    (false, true) => i18n::t!(" Puts the matched part into ${{{name}}}."),
                    (true, false) => i18n::t!(" On a match, puts the line found into ${{{name}}}."),
                    (true, true) => {
                        i18n::t!(" On a match, puts the matched part into ${{{name}}}.")
                    }
                });
            }
        };
        match self.kind {
            StepKind::WaitFor => {
                // What a wait looks at moves forward with every 送信 and
                // every wait that succeeds, so "new output" is the honest
                // description; "since the last 送信" is not, after a wait.
                let seconds = self.number.read(cx).text(cx);
                let seconds = match seconds.trim() {
                    "" => "?",
                    seconds => seconds,
                };
                let mut sentence = i18n::t!(
                    "Waits up to {seconds} s for {} to appear in new output.{}",
                    quoted(i18n::t!("the expected text")),
                    if self.continue_on_timeout {
                        i18n::t!(" If it doesn't, runs the “On Timeout” steps and moves on.")
                    } else {
                        i18n::t!(" If it doesn't, stops the macro.")
                    }
                );
                captured(&mut sentence, false);
                Some(sentence)
            }
            StepKind::If => {
                let target = match self.scope {
                    ScopeChoice::SinceLastSend => {
                        i18n::t!("the output after the last send").to_string()
                    }
                    ScopeChoice::Screen => i18n::t!("the whole screen").to_string(),
                    ScopeChoice::Variable => match variable(&self.scope_variable) {
                        Some(name) => i18n::t!("the value of ${{{name}}}"),
                        None => i18n::t!("the variable's value").to_string(),
                    },
                };
                let mut sentence = if self.regex {
                    i18n::t!(
                        "Runs “Then” if {target} matches the regular expression {}, and “Else” if not.",
                        quoted(i18n::t!("the text to match"))
                    )
                } else {
                    i18n::t!(
                        "Runs “Then” if {target} contains {}, and “Else” if not.",
                        quoted(i18n::t!("the text to match"))
                    )
                };
                if self.scope != ScopeChoice::Variable {
                    sentence.push_str(i18n::t!(" Checks right away without waiting."));
                }
                captured(&mut sentence, true);
                Some(sentence)
            }
            StepKind::SetVariable => Some(match variable(&self.capture) {
                Some(name) => i18n::t!(
                    "Sets ${{{name}}} to {}. Wherever a later step says ${{{name}}}, it becomes this value.",
                    if text.is_empty() {
                        i18n::t!("an empty string").to_string()
                    } else {
                        i18n::t!("“{text}”")
                    }
                ),
                None => i18n::t!(
                    "Enter a variable name and a value. Wherever a later step says ${{name}}, it becomes the value."
                ),
            }),
            StepKind::Send | StepKind::Sleep | StepKind::Clear | StepKind::Repeat => None,
        }
    }

    /// The variables this row itself gives a value to, children left out.
    fn own_variables(&self, cx: &App) -> Vec<String> {
        let mut names = Vec::new();
        if self.kind.names_a_variable()
            && let Ok(Some(name)) = parse_variable_name(&self.capture.read(cx).text(cx))
        {
            names.push(name);
        }
        if self.kind.has_pattern()
            && self.regex
            && let Ok(groups) = self.compiled_pattern(cx)
        {
            names.extend(groups);
        }
        names
    }

    /// Every variable this row and the rows under it give a value to.
    fn variables_set_under(&self, cx: &App, names: &mut HashSet<String>) {
        names.extend(self.own_variables(cx));
        for list in self.live_lists() {
            for child in self.list(*list) {
                child.variables_set_under(cx, names);
            }
        }
    }

    /// Whether a 繰り返し row runs its body exactly once. Anything else — a
    /// count, 0 (until cancelled) or a count not typed in properly yet — may
    /// run the body again, with what its later steps set.
    fn repeats_once(&self, cx: &App) -> bool {
        self.count.read(cx).text(cx).trim().parse::<u32>() == Ok(1)
    }

    /// Records, for this row and the rows under it, what `available` holds
    /// when the row runs, then adds what the row may set to `available`.
    ///
    /// A value set on one side of a 分岐, or by a wait that may time out and
    /// carry on, counts as set from there on: the run may well take that path,
    /// and warning about a reference that works is worse than keeping quiet
    /// about one that may not.
    fn record_reach(
        &self,
        available: &mut HashSet<String>,
        before: &mut HashMap<EntityId, HashSet<String>>,
        cx: &App,
    ) {
        before.insert(self.element_id(), available.clone());
        match self.kind {
            StepKind::Repeat => {
                if !self.repeats_once(cx) {
                    for child in &self.children {
                        child.variables_set_under(cx, available);
                    }
                }
                record_reach_of(&self.children, available, before, cx);
            }
            StepKind::If => {
                // The 「そのとき」 side runs after the match was stored, the
                // 「それ以外」 side with nothing stored.
                let mut then_side = available.clone();
                then_side.extend(self.own_variables(cx));
                record_reach_of(&self.children, &mut then_side, before, cx);
                let mut else_side = available.clone();
                record_reach_of(&self.else_children, &mut else_side, before, cx);
                available.extend(then_side);
                available.extend(else_side);
            }
            StepKind::WaitFor => {
                if self.continue_on_timeout {
                    let mut timeout_side = available.clone();
                    record_reach_of(&self.timeout_children, &mut timeout_side, before, cx);
                    available.extend(timeout_side);
                }
                available.extend(self.own_variables(cx));
            }
            StepKind::Send | StepKind::Sleep | StepKind::Clear | StepKind::SetVariable => {
                available.extend(self.own_variables(cx));
            }
        }
    }

    /// Adds every variable this row and the rows under it give a value to,
    /// with the row that does, to `defined`. A name given a value in more than
    /// one place keeps the first.
    fn collect_variables(&self, path: &StepPath, defined: &mut Vec<(String, StepPath)>, cx: &App) {
        for name in self.own_variables(cx) {
            if !defined.iter().any(|(known, _)| *known == name) {
                defined.push((name, path.clone()));
            }
        }
        for list in self.live_lists() {
            for (index, child) in self.list(*list).iter().enumerate() {
                child.collect_variables(&path.child(*list, index), defined, cx);
            }
        }
    }

    /// `${name}` references in the row's text that have no value yet when the
    /// row runs. A run leaves them as written, which is rarely what was meant.
    fn undefined_references(&self, reach: &VariableReach, cx: &App) -> Vec<Unset> {
        if !self.kind.has_text() {
            return Vec::new();
        }
        referenced_variables(&self.text.read(cx).text(cx))
            .into_iter()
            .filter_map(|name| reach.unset_at(self, name))
            .collect()
    }

    /// The 調べる変数名 of a 分岐 on a variable when it has no value yet as the
    /// branch runs. Unlike a stray `${name}` in a text field, which at least
    /// shows up as written, such a branch quietly takes 「それ以外」.
    fn undefined_scope_variable(&self, reach: &VariableReach, cx: &App) -> Option<Unset> {
        if self.kind != StepKind::If || self.scope != ScopeChoice::Variable {
            return None;
        }
        parse_variable_name(&self.scope_variable.read(cx).text(cx))
            .ok()
            .flatten()
            .and_then(|name| reach.unset_at(self, name))
    }

    /// Puts the cursor where a new row of this kind is filled in first.
    fn focus_first_field(&self, window: &mut Window, cx: &mut App) {
        let field = match self.kind {
            StepKind::SetVariable => &self.capture,
            StepKind::Send | StepKind::WaitFor | StepKind::If => &self.text,
            StepKind::Repeat => &self.count,
            StepKind::Sleep => &self.number,
            StepKind::Clear => return,
        };
        window.focus(&field.focus_handle(cx), cx);
    }

    /// The one line a folded card shows in place of its fields.
    fn summary(&self, cx: &App) -> String {
        if let Some(step) = &self.unknown {
            if let Some(problem) = step
                .unreadable_reason()
                .and_then(|reason| reason.field_problem())
            {
                return problem;
            }
            return match step.unknown_kind() {
                Some(kind) => i18n::t!("A step of a kind this version can't run (“{kind}”)"),
                None => i18n::t!("A step of a kind this version can't run").to_string(),
            };
        }
        match self.kind {
            StepKind::Sleep => {
                let seconds = self.number.read(cx).text(cx);
                match seconds.trim() {
                    "" => self.kind.description(),
                    seconds => i18n::t!("Waits {seconds} s"),
                }
            }
            StepKind::Repeat => match self.count.read(cx).text(cx).trim().parse::<u32>() {
                Ok(0) => i18n::t!("Repeats until stopped").to_string(),
                Ok(count) => i18n::tn!(count, "Repeats {count} time", "Repeats {count} times"),
                Err(_) => i18n::t!("The repeat count is empty").to_string(),
            },
            _ => {
                if let Some(description) = self.describe(cx) {
                    return description;
                }
                let text = self.text.read(cx).text(cx);
                let text = text.trim();
                if text.is_empty() {
                    return self.kind.description();
                }
                let mut summary = text.chars().take(SUMMARY_LENGTH).collect::<String>();
                if text.chars().count() > SUMMARY_LENGTH {
                    summary.push('…');
                }
                summary
            }
        }
    }
}

/// How much of a step's text a folded card shows.
const SUMMARY_LENGTH: usize = 60;

/// 「3 回」 for the count field of a 繰り返し, as its folded card and the
/// heading of its lane put it.
fn repeat_times_label(count: &str) -> String {
    match count.trim().parse::<u32>() {
        Ok(0) => i18n::t!("until stopped").to_string(),
        Ok(count) => i18n::tn!(count, "{count} time", "{count} times"),
        Err(_) => i18n::t!("no count").to_string(),
    }
}

/// Folds or unfolds every card of `rows`, including the children of lists a
/// kind switch has hidden, so switching back does not bring back a stale mix.
fn set_expanded(rows: &mut [StepRow], expanded: bool) {
    for row in rows {
        row.expanded = expanded;
        set_expanded(&mut row.children, expanded);
        set_expanded(&mut row.else_children, expanded);
        set_expanded(&mut row.timeout_children, expanded);
    }
}

fn record_reach_of(
    rows: &[StepRow],
    available: &mut HashSet<String>,
    before: &mut HashMap<EntityId, HashSet<String>>,
    cx: &App,
) {
    for row in rows {
        row.record_reach(available, before, cx);
    }
}

/// Which variables may hold a value by the time each row runs, following the
/// order the steps run in rather than only asking whether some step sets them.
struct VariableReach {
    /// Per row, by [`StepRow::element_id`].
    before: HashMap<EntityId, HashSet<String>>,
    /// Every variable some step of the form sets.
    anywhere: HashSet<String>,
}

/// A variable a row uses before anything gave it a value.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Unset {
    name: String,
    /// Whether a step that runs later sets it, which usually means the steps
    /// are in the wrong order rather than that the name is misspelt.
    set_later: bool,
}

impl VariableReach {
    fn unset_at(&self, row: &StepRow, name: String) -> Option<Unset> {
        let available = self.before.get(&row.element_id()).unwrap_or(&self.anywhere);
        (!available.contains(&name)).then(|| Unset {
            set_later: self.anywhere.contains(&name),
            name,
        })
    }
}

fn capture_placeholder() -> &'static str {
    i18n::t!("Optional")
}

fn variable_name_placeholder() -> &'static str {
    i18n::t!("Variable name")
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
fn unfiled_folder() -> &'static str {
    i18n::t!("Unfiled")
}

/// Stands in for the macros of a folder the user made but has not filled yet.
fn empty_folder_row() -> &'static str {
    i18n::t!("No macros — add one here")
}

/// The form's folder dropdown entry for 未分類.
fn no_folder() -> &'static str {
    i18n::t!("(No folder)")
}

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
    scope: ScopeChoice,
    scope_variable: String,
    continue_on_timeout: bool,
    children: Vec<StepSnapshot>,
    else_children: Vec<StepSnapshot>,
    timeout_children: Vec<StepSnapshot>,
}

impl StepRow {
    /// Every field of this row and of the rows under it, hidden lists
    /// included.
    fn collect_field_editors(&self, fields: &mut Vec<Entity<Editor>>) {
        fields.extend([
            self.text.clone(),
            self.number.clone(),
            self.count.clone(),
            self.capture.clone(),
            self.scope_variable.clone(),
        ]);
        for child in self
            .children
            .iter()
            .chain(&self.else_children)
            .chain(&self.timeout_children)
        {
            child.collect_field_editors(fields);
        }
    }

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
            scope_variable: self.scope_variable.read(cx).text(cx),
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
    /// The macro as the file held it when the form was filled or last saved.
    /// A tab stays open for a long time, and another tab, another window or a
    /// hand edit of `terminal_sessions.json` may change the same macro in the
    /// meantime; comparing the file against this is how the tab tells such a
    /// change from its own, so that saving does not silently write over it.
    base_macro: Option<TerminalMacro>,
    /// Whether the [`UNFILED_FOLDER`] heading is folded shut. A real folder
    /// keeps that in the file; this one has nowhere to live, so it lasts as
    /// long as the tab.
    unfiled_collapsed: bool,
    /// Folder whose name is being edited in the tree, with the field holding
    /// the new name.
    renaming_folder: Option<(String, Entity<Editor>)>,
    new_folder: Option<NewFolderEntry>,
    steps: Vec<StepRow>,
    /// The form's scroll position, so a step added at the bottom of a long
    /// macro can be brought into view.
    form_scroll: ScrollHandle,
    /// Whether the 「分岐と変数の使い方」 panel is open.
    show_help: bool,
    /// Whether the saved-macro tree is drawn beside the form. Hiding it gives
    /// the form, and the side-by-side lanes of a 分岐, the tab's full width.
    /// Follows the tab's width until the user picks with the button.
    show_macro_list: bool,
    /// Set once the user has shown or hidden the tree with its button, after
    /// which resizing the tab no longer overrides that choice.
    macro_list_chosen: bool,
    /// The editor's own width as last laid out. A tab shares the window with
    /// docks and splits, so the window's width says little about its own.
    measured_width: Rc<Cell<Option<Pixels>>>,
    /// The width of the column the steps are laid out in, as last laid out.
    measured_form_width: Rc<Cell<Option<Pixels>>>,
    /// Whether a redraw by the widths just measured is already on its way.
    redraw_requested: Rc<Cell<bool>>,
    /// Scrolls the form while a dragged card is held near its top or bottom
    /// edge; dropped to stop.
    drag_scroll: Option<Task<()>>,
    /// Macro a restored tab had open, loaded once `terminal_sessions.json` has
    /// been read.
    restore_key: Option<MacroKey>,
    /// The macro the tab's title last named and whether it showed the unsaved
    /// mark, so the pane is told only when either changes.
    reported_tab: (Option<MacroKey>, bool),
    /// One per text field of the form, by the field's entity. Inside a pane
    /// the editor is not drawn again when only a field's own editor changes,
    /// yet the warnings, the summaries and the unsaved mark all follow what is
    /// typed.
    field_subscriptions: HashMap<EntityId, Subscription>,
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
    /// Whether the tab's conflict mark was last reported shown.
    reported_conflict: bool,
    /// Whether the open macro has been seen in the file under
    /// `original_key`. Only then does its absence mean that it was deleted,
    /// renamed or moved elsewhere.
    on_disk: bool,
    /// Set once the tab has asked its pane to close it for a macro gone
    /// elsewhere, so that it asks only once.
    closing_because_gone: bool,
    /// The field the tab was left from, focused again when the tab is.
    last_focused_field: Option<WeakEntity<Editor>>,
    _subscriptions: [Subscription; 4],
}

pub enum MacroEditorEvent {
    /// The tab's title or its unsaved mark may have changed.
    TabChanged,
    /// The macro the tab showed, with nothing unsaved, is no longer in the
    /// file.
    Close,
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

        let name = placeholder_editor(i18n::t!("Macro name"), window, cx);
        let tags = placeholder_editor(i18n::t!("Tags (comma-separated, optional)"), window, cx);
        let description = placeholder_editor(i18n::t!("Description (optional)"), window, cx);
        let repeat = cx.new(|cx| Editor::single_line(window, cx));
        let repeat_interval = cx.new(|cx| Editor::single_line(window, cx));
        let list_filter =
            placeholder_editor(i18n::t!("Filter by folder, name, or tag"), window, cx);

        let focus_handle = cx.focus_handle();
        // `terminal_sessions.json` is read asynchronously, so an editor opened
        // before the first read landed would otherwise show an empty tree of
        // saved macros for as long as it stays open.
        let store_loaded = cx.observe_in(&store, window, |this, _, window, cx| {
            this.restore_macro(window, cx);
            this.follow_change_made_elsewhere(window, cx);
            this.close_if_gone_elsewhere(cx);
            // The pane draws the conflict mark from its own render, which a
            // change in the file alone does not cause.
            this.report_tab_change(cx);
            cx.notify();
        });
        // The pane focuses the tab itself; the form is typed into from the
        // field the user left it in, or from its name.
        let tab_focused = cx.on_focus(&focus_handle, window, |this, window, cx| {
            let field = this
                .last_focused_field
                .as_ref()
                .and_then(|field| field.upgrade())
                .filter(|field| {
                    this.field_editors()
                        .iter()
                        .any(|candidate| candidate.entity_id() == field.entity_id())
                })
                .unwrap_or_else(|| this.name.clone());
            window.focus(&field.focus_handle(cx), cx);
        });
        let tab_left = cx.on_focus_out(&focus_handle, window, |this, event, _, cx| {
            let Some(blurred) = event.blurred.upgrade() else {
                return;
            };
            this.last_focused_field = this
                .field_editors()
                .into_iter()
                .find(|field| field.focus_handle(cx) == blurred)
                .map(|field| field.downgrade());
        });
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
            base_macro: None,
            unfiled_collapsed: false,
            renaming_folder: None,
            new_folder: None,
            steps: Vec::new(),
            form_scroll: ScrollHandle::new(),
            show_help: false,
            // Until the tab has been laid out, the window is the most it can
            // be given.
            show_macro_list: window.viewport_size().width
                >= rems(MACRO_LIST_MIN_WIDTH).to_pixels(window.rem_size()),
            macro_list_chosen: false,
            measured_width: Rc::default(),
            measured_form_width: Rc::default(),
            redraw_requested: Rc::default(),
            drag_scroll: None,
            restore_key: None,
            reported_tab: (None, false),
            field_subscriptions: HashMap::default(),
            error: None,
            pending_delete: None,
            pending_folder_delete: None,
            pending_selection: None,
            focus_handle,
            reported_conflict: false,
            on_disk: false,
            closing_because_gone: false,
            last_focused_field: None,
            _subscriptions: [store_loaded, filter_edited, tab_focused, tab_left],
        };
        this.fill_form(edited, window, cx);
        this.reported_tab = (this.original_key.clone(), this.has_unsaved_edits(cx));
        this
    }

    /// A tab brought back on restart: it reopens the macro it had open, by
    /// key, once the file has been read. What was typed and not saved is not
    /// kept, so the tab opens the macro as saved.
    fn restored(
        store: Entity<TerminalSessionStore>,
        key: Option<MacroKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self::new(store, None, window, cx);
        this.restore_key = key;
        this.restore_macro(window, cx);
        this
    }

    fn restore_macro(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restore_key.is_none() || !self.store.read(cx).is_loaded() {
            return;
        }
        let Some(key) = self.restore_key.take() else {
            return;
        };
        // Something typed while the file was still being read is kept: the
        // tab would rather not reopen the old macro than throw that away.
        if self.has_unsaved_edits(cx) {
            return;
        }
        let restored = self
            .store
            .read(cx)
            .macros()
            .iter()
            .find(|candidate| candidate.key() == key)
            .cloned();
        match restored {
            Some(restored) => self.fill_form(Some(restored), window, cx),
            None => log::info!(
                "macro editor: the macro a restored tab had open is no longer saved: {:?}",
                key
            ),
        }
    }

    /// The open macro as the file holds it now, when that is no longer what
    /// the form was filled from. The folder and the name are left out of the
    /// comparison: renaming or moving a folder from this tab carries the open
    /// macro along under its new key without changing it.
    fn macro_changed_elsewhere(&self, cx: &App) -> Option<TerminalMacro> {
        let key = self.original_key.as_ref()?;
        let base = self.base_macro.as_ref()?;
        let store = self.store.read(cx);
        if !store.is_loaded() {
            return None;
        }
        let stored = store
            .macros()
            .iter()
            .find(|candidate| candidate.key() == *key)?;
        let content = |terminal_macro: &TerminalMacro| TerminalMacro {
            name: String::new(),
            folder: None,
            ..terminal_macro.clone()
        };
        (content(stored) != content(base)).then(|| stored.clone())
    }

    fn stored_under_original_key(&self, cx: &App) -> Option<bool> {
        let key = self.original_key.as_ref()?;
        let store = self.store.read(cx);
        store.is_loaded().then(|| {
            store
                .macros()
                .iter()
                .any(|candidate| candidate.key() == *key)
        })
    }

    fn note_whether_on_disk(&mut self, cx: &App) {
        if self.stored_under_original_key(cx) == Some(true) {
            self.on_disk = true;
        }
    }

    /// Whether the open macro, once in the file, has since been deleted,
    /// renamed or moved by another tab, another window or a hand edit. Saving
    /// then makes it again under the form's name, which is asked first.
    fn macro_gone_elsewhere(&self, cx: &App) -> bool {
        self.on_disk && self.stored_under_original_key(cx) == Some(false)
    }

    /// A tab with nothing unsaved over a macro that is gone has nothing left
    /// to show, so it closes. One with edits stays, marked, until they are
    /// saved or thrown away.
    fn close_if_gone_elsewhere(&mut self, cx: &mut Context<Self>) {
        self.note_whether_on_disk(cx);
        if self.closing_because_gone || !self.macro_gone_elsewhere(cx) || self.has_unsaved_edits(cx)
        {
            return;
        }
        self.closing_because_gone = true;
        cx.emit(MacroEditorEvent::Close);
    }

    /// Shows what another tab, window or a hand edit saved for the open
    /// macro, unless the form holds edits of its own: those are kept, and
    /// saving them asks before writing over the other change.
    fn follow_change_made_elsewhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stored) = self.macro_changed_elsewhere(cx) else {
            return;
        };
        if self.has_unsaved_edits(cx) {
            return;
        }
        self.fill_form(Some(stored), window, cx);
    }

    /// 「保存」 below the form. Saving over a change made elsewhere since the
    /// form was filled asks first, the way the pane asks for a file changed
    /// on disk.
    fn save_from_button(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = if self.macro_gone_elsewhere(cx) {
            let name = self.name.read(cx).text(cx).trim().to_string();
            let name = match &self.form_folder {
                Some(folder) => format!("{folder} / {name}"),
                None => name,
            };
            window.prompt(
                PromptLevel::Warning,
                &i18n::t!(
                    "This macro has been deleted or renamed elsewhere. Saving creates it again as “{name}”."
                ),
                None,
                &[i18n::t!("Save"), i18n::t!("Cancel")],
                cx,
            )
        } else if self.macro_changed_elsewhere(cx).is_some() {
            window.prompt(
                PromptLevel::Warning,
                i18n::t!(
                    "This macro has been changed elsewhere since this tab opened it. Do you want to overwrite it?"
                ),
                None,
                &[i18n::t!("Overwrite"), i18n::t!("Cancel")],
                cx,
            )
        } else {
            // A refused or failed save is shown in the form's error line,
            // which is all a click has to report.
            self.save_form(cx).detach();
            return;
        };
        cx.spawn(async move |this, cx| {
            if answer.await.ok() != Some(0) {
                return;
            }
            this.update(cx, |this, cx| this.save_form(cx).detach())
                .log_err();
        })
        .detach();
    }

    /// Shows or hides the saved-macro tree as the user asked with its button.
    fn set_macro_list_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.show_macro_list = shown;
        self.macro_list_chosen = true;
        cx.notify();
    }

    /// Points the form at `edited`, or at a blank new macro when it is `None`,
    /// and puts the cursor in its name.
    fn load_macro(
        &mut self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fill_form(edited, window, cx);
        window.focus(&self.name.focus_handle(cx), cx);
    }

    /// [`Self::load_macro`] without moving the focus, for a tab restored in
    /// the background.
    fn fill_form(
        &mut self,
        edited: Option<TerminalMacro>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edited = edited.unwrap_or_default();
        // A macro with no name yet stands for nothing on disk, which is how the
        // tree hands the form 「このフォルダーに新しいマクロ」.
        self.original_key = (!edited.name.is_empty()).then(|| edited.key());
        self.base_macro = self.original_key.is_some().then(|| edited.clone());
        self.on_disk = false;
        self.note_whether_on_disk(cx);
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
        set_expanded(&mut self.steps, false);
        self.error = None;
        self.pending_delete = None;
        self.pending_folder_delete = None;
        self.pending_selection = None;
        self.saved_snapshot = Some(self.snapshot(cx));
        // The handle outlives the macro it scrolled, so a macro opened after
        // one that was scrolled to its end would otherwise open at its end.
        self.form_scroll.set_offset(gpui::Point::default());
        cx.notify();
    }

    /// The heading over the form, which names the open macro — the only
    /// place that does while the tree is hidden.
    fn form_heading(&self) -> String {
        match &self.original_key {
            Some((_, name)) => i18n::t!("Edit “{name}”"),
            None => i18n::t!("New Macro").to_string(),
        }
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

    /// Every text field of the editor.
    fn field_editors(&self) -> Vec<Entity<Editor>> {
        let mut fields = vec![
            self.name.clone(),
            self.tags.clone(),
            self.description.clone(),
            self.repeat.clone(),
            self.repeat_interval.clone(),
            self.list_filter.clone(),
        ];
        fields.extend(
            self.renaming_folder
                .as_ref()
                .map(|(_, field)| field.clone()),
        );
        fields.extend(self.new_folder.as_ref().map(|entry| entry.field.clone()));
        for row in &self.steps {
            row.collect_field_editors(&mut fields);
        }
        fields
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
        let base = if base.is_empty() {
            i18n::t!("Macro")
        } else {
            base
        };
        let folder = self.form_folder.as_deref();
        let taken = |candidate: &str| self.store.read(cx).macro_named(folder, candidate).is_some();
        let candidate = i18n::t!("{base} (copy)");
        if !taken(&candidate) {
            return candidate;
        }
        // There are only as many taken names as there are macros, so one of
        // these suffixes is always free.
        let limit = self.store.read(cx).macros().len() + 2;
        for suffix in 2..=limit {
            let numbered = i18n::t!("{base} (copy {suffix})");
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
            StepKind::Clear => MacroStep::Clear {},
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
            StepKind::SetVariable => MacroStep::SetVariable {
                name: String::new(),
                value: String::new(),
            },
        }
    }

    #[cfg(test)]
    fn add_step(&mut self, kind: StepKind, window: &mut Window, cx: &mut Context<Self>) {
        let index = self.steps.len();
        self.insert_step(
            InsertionPoint {
                parent: None,
                index,
            },
            kind,
            window,
            cx,
        );
    }

    /// Appends a step to one of the child lists of the row at `parent`.
    #[cfg(test)]
    fn add_child_step(
        &mut self,
        parent: &StepPath,
        list: StepList,
        kind: StepKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.row(parent).map(|row| row.list(list).len()) else {
            return;
        };
        self.insert_step(
            InsertionPoint {
                parent: Some((parent.clone(), list)),
                index,
            },
            kind,
            window,
            cx,
        );
    }

    /// Puts a blank step of `kind` at `at`, unfolded and with the cursor in its
    /// first field.
    fn insert_step(
        &mut self,
        at: InsertionPoint,
        kind: StepKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = Self::blank_step(kind);
        let row = StepRow::new(&step, window, cx);
        let appended_to_macro = at.parent.is_none() && at.index >= self.steps.len();
        let Some(lane) = self.lane_mut(&at.parent) else {
            log::warn!(
                "macro editor: no list at {:?} to put a step into",
                at.parent.as_ref().map(|(path, list)| (path.label(), *list))
            );
            return;
        };
        if at.index > lane.len() {
            log::warn!(
                "macro editor: a step put in at {} of a list of {} steps",
                at.index,
                lane.len()
            );
            return;
        }
        row.focus_first_field(window, cx);
        lane.insert(at.index, row);
        // A step added at the end of the macro lands under everything else,
        // which on a long macro is below the part of the form that is
        // showing. Anywhere else it goes where the 「+」 that was just
        // clicked is, which is on screen already.
        if appended_to_macro {
            self.form_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// すべて展開 / すべて折りたたむ.
    fn set_all_expanded(&mut self, expanded: bool, cx: &mut Context<Self>) {
        set_expanded(&mut self.steps, expanded);
        cx.notify();
    }

    /// The list `parent` names: the macro's own steps for `None`.
    fn lane(&self, parent: &Option<(StepPath, StepList)>) -> Option<&Vec<StepRow>> {
        match parent {
            Some((path, list)) => Some(self.row(path)?.list(*list)),
            None => Some(&self.steps),
        }
    }

    fn lane_mut(&mut self, parent: &Option<(StepPath, StepList)>) -> Option<&mut Vec<StepRow>> {
        match parent {
            Some((path, list)) => Some(self.row_mut(path)?.list_mut(*list)),
            None => Some(&mut self.steps),
        }
    }

    /// Moves the row at `from`, with everything under it, to `to`. `to` is
    /// read as the form stands before the move, which is how the 「+」 it was
    /// dropped on was drawn. Refused (`false`) when `to` is inside the row
    /// itself or would leave the row where it is.
    fn move_row(&mut self, from: &StepPath, to: InsertionPoint) -> bool {
        if !to.accepts(from) {
            return false;
        }
        let Some((from_lane, from_index)) = from.lane_position() else {
            return false;
        };
        let target_exists = self
            .lane(&to.parent)
            .is_some_and(|lane| to.index <= lane.len());
        if !target_exists || self.row(from).is_none() {
            return false;
        }

        // Taking the row out shifts up whatever came after it in its list, the
        // list the drop lands in included when it sits under one of those
        // rows.
        let depth = from.depth() - 1;
        let target_parent = to.parent.map(|(parent, list)| {
            let mut segments = parent.0.as_ref().clone();
            if segments.len() > depth
                && segments[..depth] == from.0[..depth]
                && segments[depth].0 == from.0[depth].0
                && segments[depth].1 > from_index
            {
                segments[depth].1 -= 1;
            }
            (StepPath(Rc::new(segments)), list)
        });
        let target_index = if from_lane == target_parent && to.index > from_index {
            to.index - 1
        } else {
            to.index
        };

        let Some(row) = self
            .lane_mut(&from_lane)
            .and_then(|lane| (from_index < lane.len()).then(|| lane.remove(from_index)))
        else {
            return false;
        };
        match self.lane_mut(&target_parent) {
            Some(lane) if target_index <= lane.len() => {
                lane.insert(target_index, row);
                true
            }
            _ => {
                log::error!("macro editor: the drop target vanished while moving a step");
                if let Some(lane) = self.lane_mut(&from_lane) {
                    lane.insert(from_index.min(lane.len()), row);
                }
                false
            }
        }
    }

    /// Swaps the row at `path` with its neighbour, for 「上へ移動」 and
    /// 「下へ移動」.
    fn move_step_by_one(&mut self, path: &StepPath, up: bool) -> bool {
        let index = path.index();
        let Some(siblings) = self.siblings_mut(path) else {
            return false;
        };
        let Some(target) = (if up {
            index.checked_sub(1)
        } else {
            Some(index + 1)
        }) else {
            return false;
        };
        if index >= siblings.len() || target >= siblings.len() {
            return false;
        }
        siblings.swap(index, target);
        true
    }

    fn delete_step(&mut self, path: &StepPath) -> bool {
        let index = path.index();
        let Some(siblings) = self.siblings_mut(path) else {
            return false;
        };
        if index >= siblings.len() {
            return false;
        }
        siblings.remove(index);
        true
    }

    /// Every variable the form gives a value to, with the first row that does,
    /// in step order.
    fn defined_variables(&self, cx: &App) -> Vec<(String, StepPath)> {
        let mut defined = Vec::new();
        for (index, row) in self.steps.iter().enumerate() {
            row.collect_variables(&StepPath::top(index), &mut defined, cx);
        }
        defined
    }

    /// What each row can count on having a value when it runs. A macro that
    /// runs its steps more than once gives every step what any step set on
    /// the pass before.
    fn variable_reach(&self, defined: &[(String, StepPath)], cx: &App) -> VariableReach {
        let anywhere = defined
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        let runs_once = self.repeat.read(cx).text(cx).trim().parse::<u32>() == Ok(1);
        let mut available = if runs_once {
            HashSet::default()
        } else {
            anywhere.clone()
        };
        let mut before = HashMap::default();
        record_reach_of(&self.steps, &mut available, &mut before, cx);
        VariableReach { before, anywhere }
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
                    this.error = Some(format!("{what}: {error}").into());
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
            self.error = Some(loading_error().into());
            cx.notify();
            return;
        }
        let store = self.store.clone();
        if let Some(write) = write_folder_collapsed(&store, &folder, !collapsed, cx) {
            self.report_write_failure(
                write,
                i18n::t!("Couldn't save whether the folder is open"),
                cx,
            );
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
            editor.set_placeholder_text(i18n::t!("Folder name"), window, cx);
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
            self.error = Some(i18n::t!("Enter a folder name.").into());
            self.new_folder = Some(entry);
            cx.notify();
            return;
        };
        // Writing before the first read would rewrite the file from an empty
        // in-memory copy, dropping every macro and connection already in it.
        if !self.store.read(cx).is_loaded() {
            self.error = Some(loading_error().into());
            self.new_folder = Some(entry);
            cx.notify();
            return;
        }
        if self.store.read(cx).file().has_folder(&name) {
            self.error = Some(i18n::t!("A folder with this name already exists.").into());
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
        self.report_write_failure(write, i18n::t!("Couldn't create the folder"), cx);
        cx.notify();
    }

    /// Moves a folder one place up or down, which is all the order the tree and
    /// the picker follow: the position in `macro_folders` is the display order.
    fn move_folder(&mut self, folder: &str, up: bool, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_loaded() {
            self.error = Some(loading_error().into());
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
        self.report_write_failure(write, i18n::t!("Couldn't move the folder"), cx);
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
            self.error = Some(loading_error().into());
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
            self.report_write_failure(write, i18n::t!("Couldn't delete the folder"), cx);
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
        self.report_write_failure(write, i18n::t!("Couldn't delete the folder"), cx);
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
            self.error = Some(i18n::t!("Enter a folder name.").into());
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
            self.error = Some(loading_error().into());
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
            self.error = Some(i18n::t!("A folder with this name already exists.").into());
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
        self.report_write_failure(write, i18n::t!("Couldn't rename the folder"), cx);
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
            self.error = Some(loading_error().into());
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
        self.report_write_failure(write, i18n::t!("Couldn't move the folder"), cx);
        cx.notify();
    }

    /// Shows `message` in the form's error line, and fails the save with it so
    /// that closing the tab through 「保存」 stops at the form.
    fn refuse_save(&mut self, message: String, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        self.error = Some(message.clone().into());
        cx.notify();
        Task::ready(Err(anyhow::anyhow!(message)))
    }

    /// Writes the form to `terminal_sessions.json`. The tab stays open on the
    /// macro just saved, which from then on is what counts as unchanged.
    fn save_form(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        // Writing before the first read would rewrite the file from an empty
        // in-memory copy, dropping every macro and connection already in it.
        if !self.store.read(cx).is_loaded() {
            return self.refuse_save(loading_error().to_string(), cx);
        }

        let name = self.name.read(cx).text(cx).trim().to_string();
        if name.is_empty() {
            return self.refuse_save(i18n::t!("Enter a macro name.").to_string(), cx);
        }

        let mut steps = Vec::with_capacity(self.steps.len());
        for (index, row) in self.steps.iter().enumerate() {
            match row.to_step(&(index + 1).to_string(), cx) {
                Ok(step) => steps.push(step),
                Err(message) => return self.refuse_save(message, cx),
            }
        }

        let Ok(repeat) = self.repeat.read(cx).text(cx).trim().parse::<u32>() else {
            return self.refuse_save(
                i18n::t!("Enter the repeat count as a whole number, 0 or more.").to_string(),
                cx,
            );
        };
        let Some(repeat_interval_ms) = parse_seconds(&self.repeat_interval.read(cx).text(cx))
        else {
            return self.refuse_save(
                i18n::t!("Enter the repeat interval in seconds, 0 or more.").to_string(),
                cx,
            );
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
            return self.refuse_save(
                i18n::t!("This folder already has a macro with this name.").to_string(),
                cx,
            );
        }

        // Taken now rather than once the write lands: anything typed in the
        // meantime is not in the file, so it still counts as unsaved.
        let saved_snapshot = self.snapshot(cx);
        let saved_key = key.clone();
        // Set before the store is told, so that its observers already compare
        // the file against what this tab has just written.
        self.base_macro = Some(terminal_macro.clone());
        // The store takes the new file at once, whether or not the write to
        // disk then succeeds; a rename read against the old key would look
        // like the macro had gone elsewhere.
        self.original_key = Some(key.clone());
        self.on_disk = true;
        self.closing_because_gone = false;
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
                this.update(cx, |this, cx| {
                    this.original_key = Some(saved_key);
                    this.saved_snapshot = Some(saved_snapshot);
                    this.error = None;
                    cx.notify();
                })?;
                Ok(())
            }
            Err(error) => {
                let message = i18n::t!("Couldn't save: {error}");
                this.update(cx, |this, cx| {
                    this.error = Some(message.clone().into());
                    cx.notify();
                })
                .ok();
                Err(anyhow::anyhow!(message))
            }
        })
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
            self.error = Some(loading_error().into());
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

        self.report_write_failure(write, i18n::t!("Couldn't delete"), cx);
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
            return Label::new(i18n::t!("Loading…"))
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }

        let rows = self.macro_tree(cx);
        if rows.is_empty() {
            let filtered = !self.list_filter.read(cx).text(cx).trim().is_empty();
            return Label::new(if filtered {
                i18n::t!("No macros match the filter")
            } else {
                i18n::t!("No saved macros")
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
                            i18n::t!("Rename to this name"),
                            i18n::t!("Cancel renaming"),
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
                                Label::new(empty_folder_row())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    );
                }
                MacroTreeRow::Macro(terminal_macro) => {
                    let key = terminal_macro.key();
                    let name = terminal_macro.name.clone();
                    let selected = editing.as_ref() == Some(&key);
                    let detail =
                        i18n::tn!(terminal_macro.steps.len(), "{count} step", "{count} steps");
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
        let heading = folder
            .clone()
            .unwrap_or_else(|| unfiled_folder().to_string());
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
                        .tooltip(Tooltip::text(i18n::t!(
                            "Renames the folder (the macros in it move along)"
                        )))
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
                    menu = menu.entry(i18n::t!("New Macro in Folder"), None, {
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
                    menu = menu.entry(i18n::t!("Rename Folder"), None, {
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
                        menu = menu.entry(i18n::t!("Move Up"), None, {
                            let name = name.clone();
                            let view = view.clone();
                            move |_, cx| {
                                view.update(cx, |this, cx| this.move_folder(&name, true, cx))
                                    .ok();
                            }
                        });
                    }
                    if position + 1 < folder_count {
                        menu = menu.entry(i18n::t!("Move Down"), None, {
                            let name = name.clone();
                            let view = view.clone();
                            move |_, cx| {
                                view.update(cx, |this, cx| this.move_folder(&name, false, cx))
                                    .ok();
                            }
                        });
                    }
                    menu.separator().entry(i18n::t!("Delete…"), None, {
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
            .unwrap_or_else(|| no_folder().to_string());
        let view = cx.weak_entity();
        let menu = ContextMenu::build(window, cx, move |mut menu, _, _| {
            menu = menu.entry(no_folder(), None, {
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
            menu.separator().entry(i18n::t!("New Folder…"), None, {
                move |window, cx| {
                    view.update(cx, |this, cx| this.begin_new_folder(true, window, cx))
                        .ok();
                }
            })
        });

        DropdownMenu::new("macro-folder-choices", selected, menu).style(DropdownStyle::Outlined)
    }

    /// One child list of a card — the body of a 繰り返し, a side of a 分岐, or
    /// the 「タイムアウト時」 steps of a 期待文字を待つ — as a lane of its own
    /// under a heading. `width` is what the lane has to itself.
    #[allow(clippy::too_many_arguments)]
    fn render_child_list(
        &self,
        path: &StepPath,
        list: StepList,
        heading: String,
        width: Pixels,
        reach: &VariableReach,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.row(path) else {
            return div().into_any_element();
        };
        let child_count = row.list(list).len();
        let colors = cx.theme().colors();
        let selector = format!("macro-lane-{}", lane_key(&Some((path.clone(), list))));
        let frame = v_flex()
            .w_full()
            .min_w_0()
            .rounded_sm()
            .border_1()
            .border_color(colors.border)
            .debug_selector(move || selector);
        // Deeper levels stay in the row tree and are written back untouched;
        // only the form stops drawing them, because the indentation (and the
        // step numbers) stop being readable.
        if path.depth() >= MAX_EDITABLE_STEP_DEPTH {
            return frame
                .p_1()
                .child(
                    Label::new(i18n::tn!(
                        child_count,
                        "{heading}: {count} step (edit deeper nesting in the JSON file)",
                        "{heading}: {count} steps (edit deeper nesting in the JSON file)"
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .into_any_element();
        }
        let heading = div()
            .w_full()
            .px_1()
            .py_0p5()
            .rounded_t_sm()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.surface_background)
            .child(
                Label::new(heading)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            );
        frame
            .child(heading)
            .child(div().w_full().p_1().child(self.render_lane(
                Some((path.clone(), list)),
                (width - lane_chrome(window.rem_size())).max(px(0.)),
                reach,
                window,
                cx,
            )))
            .into_any_element()
    }

    /// The cards of one list joined by a line, with a 「+」 before each card
    /// and one at the end: each is where a new step goes in and where a
    /// dragged card can be dropped. `width` is what each card has.
    fn render_lane(
        &self,
        parent: Option<(StepPath, StepList)>,
        width: Pixels,
        reach: &VariableReach,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = self.lane(&parent).map_or(0, Vec::len);
        let mut lane = v_flex().w_full().min_w_0().items_center();
        for index in 0..count {
            let point = InsertionPoint {
                parent: parent.clone(),
                index,
            };
            lane = lane
                .child(self.render_insertion_point(&point, false, cx))
                .child(self.render_step(&point.path(), width, reach, window, cx));
        }
        lane.child(self.render_insertion_point(
            &InsertionPoint {
                parent,
                index: count,
            },
            true,
            cx,
        ))
        .into_any_element()
    }

    /// A 「+」 on the line between two cards, or under the last one when
    /// `last` is set: a menu of the step kinds to put in there, and a drop
    /// target for a card being dragged.
    fn render_insertion_point(
        &self,
        point: &InsertionPoint,
        last: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        // A shade past the card borders, so the line reads as joining the
        // cards rather than as more of their outline.
        let (line, drop_background, drop_border) = (
            colors.text_muted,
            colors.drop_target_background,
            colors.drop_target_border,
        );
        let empty_lane = last && point.index == 0;
        let selector = point.debug_selector();
        let id_prefix = SharedString::from(format!("step-insert-{}", lane_key(&point.parent)));
        let accepted = point.clone();
        let dropped = point.clone();
        let offered = point.clone();
        let view = cx.weak_entity();
        let connector = || div().w(px(2.)).h(px(6.)).bg(line);
        v_flex()
            .id((id_prefix, point.index))
            .w_full()
            .items_center()
            .rounded_sm()
            .border_1()
            .border_color(gpui::transparent_black())
            .debug_selector(move || selector)
            .can_drop(move |dragged, _, _| {
                dragged
                    .downcast_ref::<DraggedStep>()
                    .is_some_and(|dragged| accepted.accepts(&dragged.path))
            })
            .drag_over::<DraggedStep>(move |style, _, _, _| {
                style.bg(drop_background).border_color(drop_border)
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedStep, _, cx| {
                // The path was taken when the drag began; if the rows have
                // changed since, it may name some other row now.
                if this.row(&dragged.path).map(StepRow::element_id) != Some(dragged.row) {
                    log::warn!("macro editor: dropped a step whose row has moved since the drag");
                    return;
                }
                if this.move_row(&dragged.path, dropped.clone()) {
                    cx.notify();
                }
            }))
            .child(connector())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        PopoverMenu::new("step-insert-menu")
                            .trigger_with_tooltip(
                                IconButton::new("step-insert-button", IconName::Plus)
                                    .icon_size(IconSize::XSmall)
                                    .style(ButtonStyle::Outlined),
                                Tooltip::text(i18n::t!("Add a step here")),
                            )
                            .menu(move |window, cx| {
                                let view = view.clone();
                                let point = offered.clone();
                                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                                    for kind in StepKind::ALL {
                                        let view = view.clone();
                                        let point = point.clone();
                                        menu = menu.item(
                                            ContextMenuEntry::new(kind.label())
                                                .icon(kind.icon())
                                                .icon_color(Color::Muted)
                                                .handler(move |window, cx| {
                                                    let point = point.clone();
                                                    view.update(cx, |this, cx| {
                                                        this.insert_step(point, kind, window, cx);
                                                    })
                                                    .ok();
                                                })
                                                .documentation_aside(
                                                    DocumentationSide::Right,
                                                    move |_| {
                                                        Label::new(kind.description())
                                                            .size(LabelSize::Small)
                                                            .into_any_element()
                                                    },
                                                ),
                                        );
                                    }
                                    menu
                                }))
                            }),
                    )
                    .when(empty_lane, |this| {
                        this.child(
                            Label::new(i18n::t!("Add Step"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(!last, |this| this.child(connector()))
            .into_any_element()
    }

    /// The heading over the step list: the 「分岐と変数の使い方」 toggle, the
    /// panel it opens, and the variables the macro has so far.
    fn render_steps_header(
        &self,
        defined: &[(String, StepPath)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let variables = if defined.is_empty() {
            i18n::t!("Variables: none yet (create them with “Set Variable” or “Capture Into”)")
                .to_string()
        } else {
            let listed = defined
                .iter()
                .map(|(name, path)| i18n::t!("${{{name}}} (step {})", path.label()))
                .collect::<Vec<_>>()
                .join(i18n::tc!("list separator", ", "));
            i18n::t!("Variables: {listed}")
        };
        let colors = cx.theme().colors();
        let (border, background) = (colors.border, colors.surface_background);
        v_flex()
            .w_full()
            .gap_1()
            .pt_1()
            .border_t_1()
            .border_color(border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .child(
                                Label::new(i18n::t!("Steps ({})", self.steps.len()))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Button::new("macro-expand-all", i18n::t!("Expand All"))
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text(i18n::t!(
                                        "Shows the fields of every step"
                                    )))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_all_expanded(true, cx);
                                    })),
                            )
                            .child(
                                Button::new("macro-collapse-all", i18n::t!("Collapse All"))
                                    .label_size(LabelSize::Small)
                                    .tooltip(Tooltip::text(i18n::t!(
                                        "Shows only the heading and summary of every step"
                                    )))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_all_expanded(false, cx);
                                    })),
                            ),
                    )
                    .child(
                        Button::new(
                            "macro-help",
                            if self.show_help {
                                i18n::t!("Close Help")
                            } else {
                                i18n::t!("How Branches and Variables Work")
                            },
                        )
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Info).size(IconSize::XSmall))
                        .toggle_state(self.show_help)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_help = !this.show_help;
                            cx.notify();
                        })),
                    ),
            )
            .when(self.show_help, |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .p_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(border)
                        .bg(background)
                        .debug_selector(|| "macro-help-panel".into())
                        .children(
                            macro_help()
                                .into_iter()
                                .map(|paragraph| Label::new(paragraph).size(LabelSize::Small)),
                        ),
                )
            })
            .child(
                Label::new(variables)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    /// What is wrong with the row's own fields, in the order the expanded card
    /// lists it: a folded card sums it up as one icon in its header.
    fn step_notes(&self, row: &StepRow, reach: &VariableReach, cx: &App) -> Vec<(String, Color)> {
        let mut notes = Vec::new();
        if let Some(error) = row.pattern_error(cx) {
            notes.push((
                i18n::t!("Invalid regular expression: {error}"),
                Color::Error,
            ));
        }
        for name in row.repeated_references(cx) {
            notes.push((
                i18n::t!(
                    "A repetition right after ${{{name}}} applies only to the last character of its value, \
                     and when the value is empty it applies to the character before it or makes the regular expression invalid. \
                     To apply it to the whole value, wrap it as (?:${{{name}}})."
                ),
                Color::Warning,
            ));
        }
        for warning in unset_reference_warnings(&row.undefined_references(reach, cx)) {
            notes.push((warning, Color::Warning));
        }
        if let Some(unset) = row.undefined_scope_variable(reach, cx) {
            let name = unset.name;
            notes.push((
                if unset.set_later {
                    i18n::t!(
                        "${{{name}}} gets its value only after this branch, so “Else” runs here."
                    )
                } else {
                    i18n::t!("No step sets ${{{name}}}, so this branch always runs “Else”.")
                },
                Color::Warning,
            ));
        }
        notes
    }

    /// 折りたたむ / 展開, 上へ移動 / 下へ移動 and 削除 for one card, behind its
    /// 「…」 button and its right-click menu alike.
    fn step_menu(
        view: WeakEntity<Self>,
        path: StepPath,
        expanded: bool,
        can_move_up: bool,
        can_move_down: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, move |menu, _, _| {
            let action = |update: fn(&mut MacroEditor, &StepPath) -> bool| {
                let view = view.clone();
                let path = path.clone();
                move |_: &mut Window, cx: &mut App| {
                    view.update(cx, |this, cx| {
                        if update(this, &path) {
                            cx.notify();
                        }
                    })
                    .ok();
                }
            };
            menu.item(
                ContextMenuEntry::new(if expanded {
                    i18n::tc!("terminal macro", "Collapse")
                } else {
                    i18n::tc!("terminal macro", "Expand")
                })
                .handler(action(|this, path| {
                    this.row_mut(path)
                        .map(|row| row.expanded = !row.expanded)
                        .is_some()
                })),
            )
            .separator()
            .item(
                ContextMenuEntry::new(i18n::t!("Move Up"))
                    .icon(IconName::ArrowUp)
                    .disabled(!can_move_up)
                    .handler(action(|this, path| this.move_step_by_one(path, true))),
            )
            .item(
                ContextMenuEntry::new(i18n::t!("Move Down"))
                    .icon(IconName::ArrowDown)
                    .disabled(!can_move_down)
                    .handler(action(|this, path| this.move_step_by_one(path, false))),
            )
            .separator()
            .item(
                ContextMenuEntry::new(i18n::t!("Delete"))
                    .icon(IconName::Trash)
                    .handler(action(|this, path| this.delete_step(path))),
            )
        })
    }

    /// One step as a card: a header (number, kind, what it does, and the
    /// handle it is dragged by), its fields while unfolded, and the lanes of
    /// the steps it holds. `width` is what the card has, which decides whether
    /// the two sides of a 分岐 fit beside each other.
    fn render_step(
        &self,
        path: &StepPath,
        width: Pixels,
        reach: &VariableReach,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self.row(path) else {
            return div().into_any_element();
        };
        let kind = row.kind;
        let element_id = row.element_id();
        let index = path.index();
        let expanded = row.expanded;
        let unknown_kind = row.unknown.as_ref().map(|step| step.unknown_kind());
        let field_problem = row
            .unknown
            .as_ref()
            .and_then(|step| step.unreadable_reason())
            .and_then(|reason| reason.field_problem());
        let sibling_count = match path.lane_position() {
            Some((parent, _)) => self.lane(&parent).map_or(0, Vec::len),
            None => 0,
        };

        // The dropdowns only exist on an unfolded card; a long folded macro
        // should not build three menus per card on every frame.
        let view = cx.weak_entity();
        let kind_menu = (expanded && unknown_kind.is_none()).then(|| {
            ContextMenu::build(window, cx, {
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
                                    // The hints name the fields, so they follow the
                                    // kind the row was just switched to.
                                    let placeholder = candidate.text_placeholder();
                                    row.text.update(cx, |editor, cx| {
                                        editor.set_placeholder_text(&placeholder, window, cx);
                                    });
                                    let placeholder = candidate.capture_placeholder();
                                    row.capture.update(cx, |editor, cx| {
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
            })
        });

        let scope_menu = (expanded && kind == StepKind::If).then(|| {
            let view = cx.weak_entity();
            let path = path.clone();
            let selected = row.scope;
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for candidate in ScopeChoice::ALL {
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

        let timeout_menu = (expanded && kind == StepKind::WaitFor).then(|| {
            let view = cx.weak_entity();
            let path = path.clone();
            let selected = row.continue_on_timeout;
            ContextMenu::build(window, cx, move |mut menu, _, _| {
                for (candidate, label) in TIMEOUT_CHOICES {
                    let view = view.clone();
                    let path = path.clone();
                    menu = menu.toggleable_entry(
                        i18n::tr(label),
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

        let inner_width = (width - card_chrome(window.rem_size())).max(px(0.));
        let lane_gap = px(8.);
        let lanes_editable = path.depth() < MAX_EDITABLE_STEP_DEPTH;
        // A 分岐 with steps on one side only gives the empty side just enough
        // for its heading and 「+」, and the rest to the side that has cards.
        let narrow_lane_width = rems(NARROW_LANE_WIDTH).to_pixels(window.rem_size());
        let narrow_side = match (row.children.is_empty(), row.else_children.is_empty()) {
            (true, false) if kind == StepKind::If => Some(StepList::Main),
            (false, true) if kind == StepKind::If => Some(StepList::Else),
            _ => None,
        };
        // Past the editable depth both sides are a line of text, which side
        // by side, and above all on a narrow side, would wrap a few words to
        // a line.
        let side_by_side = lanes_editable
            && branch_sides_fit(
                inner_width,
                narrow_side.is_some(),
                lane_gap,
                window.rem_size(),
            );
        let lanes = match kind {
            StepKind::Repeat => vec![self.render_child_list(
                path,
                StepList::Main,
                i18n::t!(
                    "Steps to Repeat ({})",
                    repeat_times_label(&row.count.read(cx).text(cx))
                ),
                inner_width,
                reach,
                window,
                cx,
            )],
            StepKind::If => {
                let wide_width = (inner_width - lane_gap - narrow_lane_width).max(px(0.));
                let [then_width, else_width] = match (side_by_side, narrow_side) {
                    (false, _) => [inner_width; 2],
                    (true, None) => [((inner_width - lane_gap) / 2.).max(px(0.)); 2],
                    (true, Some(StepList::Main)) => [narrow_lane_width, wide_width],
                    (true, Some(_)) => [wide_width, narrow_lane_width],
                };
                vec![
                    self.render_child_list(
                        path,
                        StepList::Main,
                        i18n::tc!("terminal macro", "Then").to_string(),
                        then_width,
                        reach,
                        window,
                        cx,
                    ),
                    self.render_child_list(
                        path,
                        StepList::Else,
                        i18n::tc!("terminal macro", "Else").to_string(),
                        else_width,
                        reach,
                        window,
                        cx,
                    ),
                ]
            }
            StepKind::WaitFor if row.continue_on_timeout => vec![self.render_child_list(
                path,
                StepList::Timeout,
                i18n::tc!("terminal macro", "On Timeout").to_string(),
                inner_width,
                reach,
                window,
                cx,
            )],
            _ => Vec::new(),
        };
        let lanes = (!lanes.is_empty()).then(|| {
            if lanes.len() > 1 && side_by_side {
                let narrow_index = match narrow_side {
                    Some(StepList::Main) => Some(0),
                    Some(StepList::Else) => Some(1),
                    _ => None,
                };
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(lane_gap)
                    .children(lanes.into_iter().enumerate().map(|(index, lane)| {
                        if Some(index) == narrow_index {
                            div().flex_none().w(narrow_lane_width).child(lane)
                        } else {
                            div().flex_1().min_w_0().child(lane)
                        }
                    }))
                    .into_any_element()
            } else {
                v_flex()
                    .w_full()
                    .gap(lane_gap)
                    .children(lanes)
                    .into_any_element()
            }
        });

        let description = row.describe(cx);
        let notes = self.step_notes(row, reach, cx);
        let summary = row.summary(cx);
        let kind_label: SharedString = if unknown_kind.is_some() {
            i18n::tc!("terminal macro step", "Unknown Step").into()
        } else {
            kind.label().into()
        };
        let kind_icon = if unknown_kind.is_some() {
            IconName::Warning
        } else {
            kind.icon()
        };

        let toggle_path = path.clone();
        let title_path = path.clone();
        let delete = path.clone();
        let newline_path = path.clone();
        let regex_path = path.clone();
        let selector = path.debug_selector();
        let header_selector = format!("{selector}-header");
        let title_selector = format!("{selector}-title");
        let kind_selector = format!("{selector}-kind");
        let header_buttons_selector = format!("{selector}-header-buttons");
        let notes_selector = format!("{selector}-notes");
        let can_move_up = index > 0;
        let can_move_down = index + 1 < sibling_count;
        let small_field = |editor: &Entity<Editor>, width: Rems, window: &mut Window, cx: &App| {
            input_frame_sized(
                InputFrameSize::Compact,
                editor.focus_handle(cx).contains_focused(window, cx),
                cx,
            )
            .w(width)
            .child(editor.clone())
        };

        let dragged = DraggedStep {
            path: path.clone(),
            row: element_id,
            number: path.label().into(),
            kind: kind_label.clone(),
            icon: kind_icon,
            summary: summary.clone().into(),
        };
        let folded_notes = (!expanded && !notes.is_empty()).then(|| {
            let color = if notes.iter().any(|(_, color)| *color == Color::Error) {
                Color::Error
            } else {
                Color::Warning
            };
            let tooltip = notes
                .iter()
                .map(|(note, _)| note.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            div()
                .id(("step-notes", element_id))
                .flex_none()
                .debug_selector(move || notes_selector)
                .tooltip(Tooltip::text(tooltip))
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(color),
                )
        });
        let menu_view = cx.weak_entity();
        let menu_path = path.clone();
        let header = h_flex()
            .id(("step-header", element_id))
            .w_full()
            .min_w_0()
            .gap_1()
            .debug_selector(move || header_selector)
            .on_drag(dragged, |dragged: &DraggedStep, _, _, cx| {
                let dragged = dragged.clone();
                cx.new(|_| DraggedStepHeader(dragged))
            })
            .child(div().flex_none().child(
                Disclosure::new(("step-toggle", element_id), expanded).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(row) = this.row_mut(&toggle_path) {
                            row.expanded = !row.expanded;
                            cx.notify();
                        }
                    },
                )),
            ))
            .child(
                // In a narrow column the title gives way to the buttons:
                // without `min_w_0` and the clip, the labels keep their full
                // width and are drawn over the buttons on its right.
                h_flex()
                    .id(("step-title", element_id))
                    .debug_selector(move || title_selector)
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(row) = this.row_mut(&title_path) {
                            row.expanded = !row.expanded;
                            cx.notify();
                        }
                    }))
                    .child(
                        div().flex_none().child(
                            Label::new(path.label())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .child(div().flex_none().child(
                        Icon::new(kind_icon).size(IconSize::Small).color(
                            if unknown_kind.is_some() {
                                Color::Warning
                            } else {
                                Color::Muted
                            },
                        ),
                    ))
                    .child(
                        div().debug_selector(move || kind_selector).min_w_0().child(
                            Label::new(kind_label)
                                .size(LabelSize::Small)
                                .color(if unknown_kind.is_some() {
                                    Color::Warning
                                } else {
                                    Color::Default
                                })
                                .truncate(),
                        ),
                    )
                    .when(!expanded, |this| {
                        this.child(
                            div().flex_1().min_w_0().child(
                                Label::new(summary)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .debug_selector(move || header_buttons_selector)
                    .children(folded_notes)
                    .child(
                        PopoverMenu::new(("step-menu", element_id))
                            .trigger_with_tooltip(
                                IconButton::new(("step-more", element_id), IconName::Ellipsis)
                                    .icon_size(IconSize::Small),
                                Tooltip::text(i18n::t!("Opens the menu for moving and folding")),
                            )
                            .menu({
                                let view = cx.weak_entity();
                                let path = path.clone();
                                move |window, cx| {
                                    Some(Self::step_menu(
                                        view.clone(),
                                        path.clone(),
                                        expanded,
                                        can_move_up,
                                        can_move_down,
                                        window,
                                        cx,
                                    ))
                                }
                            }),
                    )
                    .child(
                        IconButton::new(("step-delete", element_id), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!("Deletes this step")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.delete_step(&delete) {
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        div()
                            .id(("step-grip", element_id))
                            .flex_none()
                            .cursor_grab()
                            .tooltip(Tooltip::text(i18n::t!(
                                "Drag onto a “+” along or at the end of a line to move the step"
                            )))
                            .child(
                                Icon::new(IconName::Menu)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            ),
                    ),
            );
        let header = right_click_menu(("step-context-menu", element_id))
            .trigger(move |_, _, _| header)
            .menu(move |window, cx| {
                Self::step_menu(
                    menu_view.clone(),
                    menu_path.clone(),
                    expanded,
                    can_move_up,
                    can_move_down,
                    window,
                    cx,
                )
            });

        let body =
            expanded.then(|| {
                v_flex()
                    .w_full()
                    .gap_1()
                    .when_some(kind_menu, |this, kind_menu| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(i18n::t!("Kind"))
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
                    })
                    .when_some(unknown_kind, |this, unknown_kind| {
                        let message = match (field_problem, unknown_kind) {
                            (Some(problem), _) => i18n::t!(
                                "{problem}. Fix it in terminal_sessions.json. \
                                 Saving here keeps it as it was, but a macro that contains it can't be run."
                            ),
                            (None, Some(kind)) => i18n::t!(
                                "A step of a kind this version can't run (“{kind}”). \
                                 Saving keeps it as it was, but a macro that contains it can't be run."
                            ),
                            (None, None) => i18n::t!(
                                "A step of a kind this version can't run. \
                                 Saving keeps it as it was, but a macro that contains it can't be run."
                            )
                            .to_string(),
                        };
                        // Switching the kind would throw away what the later
                        // version wrote, so the card only offers moving and
                        // deleting.
                        this.child(
                            Label::new(message)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .when_some(description, |this, description| {
                        this.child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(kind == StepKind::SetVariable, |this| {
                        this.child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(
                                    Label::new(i18n::t!("Variable name"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(small_field(&row.capture, rems(10.), window, cx))
                                .child(
                                    Label::new(i18n::t!("Value"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    input_frame_sized(
                                        InputFrameSize::Compact,
                                        row.text.focus_handle(cx).contains_focused(window, cx),
                                        cx,
                                    )
                                    .flex_1()
                                    .min_w_0()
                                    .child(row.text.clone()),
                                ),
                        )
                    })
                    .when(kind.has_text() && kind != StepKind::SetVariable, |this| {
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
                                .label(i18n::t!("Send Enter at the End"))
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
                                    Label::new(i18n::t!("Repeat Count"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(small_field(&row.count, rems(4.), window, cx))
                                .child(
                                    Label::new(i18n::t!("(0 = repeat until stopped)"))
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
                                        i18n::t!("Timeout (seconds)")
                                    } else {
                                        i18n::t!("Seconds")
                                    })
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                )
                                .child(small_field(&row.number, rems(6.), window, cx)),
                        )
                    })
                    .when(kind.has_pattern(), |this| {
                        this.child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Checkbox::new(("step-regex", element_id), row.regex.into())
                                        .label(i18n::t!("Regular Expression"))
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(
                                            move |this, state: &ToggleState, _, cx| {
                                                let checked = *state == ToggleState::Selected;
                                                if let Some(row) = this.row_mut(&regex_path)
                                                    && row.regex != checked
                                                {
                                                    row.regex = checked;
                                                    cx.notify();
                                                }
                                            },
                                        )),
                                )
                                .when_some(scope_menu, |this, menu| {
                                    this.child(
                                        Label::new(i18n::tc!("terminal macro", "Target"))
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
                                })
                                .when(
                                    kind == StepKind::If && row.scope == ScopeChoice::Variable,
                                    |this| {
                                        this.child(
                                            Label::new(i18n::t!("Variable to Check"))
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .child(
                                            small_field(&row.scope_variable, rems(8.), window, cx),
                                        )
                                    },
                                ),
                        )
                    })
                    .when(kind == StepKind::WaitFor || kind == StepKind::If, |this| {
                        this.child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .child(
                                    Label::new(i18n::t!("Capture Into"))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(small_field(&row.capture, rems(8.), window, cx))
                                .when(kind == StepKind::WaitFor, |this| {
                                    this.child(
                                        Label::new(i18n::tc!("terminal macro", "On Timeout"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .when_some(
                                        timeout_menu,
                                        |this, menu| {
                                            this.child(
                                                DropdownMenu::new(
                                                    ("step-timeout", element_id),
                                                    timeout_choice_label(row.continue_on_timeout),
                                                    menu,
                                                )
                                                .style(DropdownStyle::Outlined),
                                            )
                                        },
                                    )
                                }),
                        )
                    })
                    .children(
                        notes.into_iter().map(|(note, color)| {
                            Label::new(note).size(LabelSize::Small).color(color)
                        }),
                    )
            });

        let colors = cx.theme().colors();
        let stripe = if unknown_kind.is_some() {
            Color::Error
        } else {
            kind.stripe_color()
        };
        let has_content = body.is_some() || lanes.is_some();
        h_flex()
            .w_full()
            .min_w_0()
            .items_stretch()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .debug_selector(move || selector)
            .child(
                div()
                    .flex_none()
                    .w(CARD_STRIPE_WIDTH)
                    .rounded_l_md()
                    .bg(stripe.color(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .w_full()
                            .px_1p5()
                            .py_1()
                            .rounded_tr_md()
                            .bg(colors.element_background)
                            .when(has_content, |this| {
                                this.border_b_1().border_color(colors.border)
                            })
                            .when(!has_content, |this| this.rounded_br_md())
                            .child(header),
                    )
                    .when(has_content, |this| {
                        this.child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .p_1p5()
                                .children(body)
                                .children(lanes),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// The kind-colored stripe down a card's left edge.
const CARD_STRIPE_WIDTH: Pixels = px(3.);

/// A card's border, stripe and padding (`p_1p5` on both sides), which its
/// lanes do not get. The padding is in rems, so it follows the UI font size.
fn card_chrome(rem_size: Pixels) -> Pixels {
    px(2.) + CARD_STRIPE_WIDTH + rems(0.75).to_pixels(rem_size)
}

/// A lane's border and padding (`p_1` on both sides), which its cards do not
/// get.
fn lane_chrome(rem_size: Pixels) -> Pixels {
    px(2.) + rems(0.5).to_pixels(rem_size)
}

/// Whether the two sides of a 分岐 fit beside each other in `inner_width`.
/// When one side is empty it only needs its narrow width, so the side with
/// cards is the one that has to get a lane's minimum.
fn branch_sides_fit(
    inner_width: Pixels,
    one_side_empty: bool,
    lane_gap: Pixels,
    rem_size: Pixels,
) -> bool {
    let min_lane_width = rems(MIN_LANE_WIDTH).to_pixels(rem_size);
    let other_side = if one_side_empty {
        rems(NARROW_LANE_WIDTH).to_pixels(rem_size)
    } else {
        min_lane_width
    };
    inner_width >= min_lane_width + lane_gap + other_side
}

/// The narrowest a lane with cards is drawn; a 分岐 whose card cannot give
/// its sides this much stacks them instead of putting them side by side.
const MIN_LANE_WIDTH: f32 = 16.;

/// The width (in rems) of the empty side of a 分岐 whose other side has steps:
/// room for its heading and its 「+」.
const NARROW_LANE_WIDTH: f32 = 8.;

/// Below this width (in rems) of the editor's tab the saved-macro tree is
/// hidden, so the form keeps a width worth editing in.
const MACRO_LIST_MIN_WIDTH: f32 = 56.;

/// What the form keeps clear on its right for its scrollbar (its `pr_3`).
const FORM_SCROLLBAR_ROOM: Pixels = px(12.);

/// How close (in rems) to the form's top or bottom edge a dragged card has
/// to be for the form to scroll.
const DRAG_SCROLL_EDGE: f32 = 3.;

/// How far the form scrolls on each tick while a card is held at its edge.
const DRAG_SCROLL_STEP: Pixels = px(8.);

const DRAG_SCROLL_INTERVAL: Duration = Duration::from_millis(16);

/// A step card dragged by its header onto a 「+」.
#[derive(Clone)]
struct DraggedStep {
    path: StepPath,
    /// [`StepRow::element_id`] of the dragged row, which the drop checks
    /// `path` still leads to.
    row: EntityId,
    number: SharedString,
    kind: SharedString,
    icon: IconName,
    summary: SharedString,
}

/// What follows the pointer while a card is dragged: a copy of its header.
struct DraggedStepHeader(DraggedStep);

impl Render for DraggedStepHeader {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dragged = &self.0;
        h_flex()
            .max_w(rems(24.))
            .gap_1()
            .px_1p5()
            .py_0p5()
            .rounded_sm()
            .elevation_2(cx)
            .child(
                Label::new(dragged.number.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Icon::new(dragged.icon)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new(dragged.kind.clone()).size(LabelSize::Small))
            .child(
                div().min_w_0().child(
                    Label::new(dragged.summary.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
    }
}

/// The warnings for the `${name}` references of a row that have no value yet:
/// one for the names nothing sets, one for the names only later steps set.
fn unset_reference_warnings(unset: &[Unset]) -> Vec<String> {
    let names = |set_later: bool| {
        unset
            .iter()
            .filter(|unset| unset.set_later == set_later)
            .map(|unset| format!("${{{}}}", unset.name))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut warnings = Vec::new();
    let nowhere = names(false);
    if !nowhere.is_empty() {
        warnings.push(i18n::t!(
            "No step sets {nowhere}. When the macro runs, it stays as written."
        ));
    }
    let later = names(true);
    if !later.is_empty() {
        warnings.push(i18n::t!(
            "{later} gets its value only after this step. Here it stays as written."
        ));
    }
    warnings
}

/// How deep the form unfolds a step tree. Rows below this stay in the macro and
/// are saved unchanged, but the indentation and the 「3-2-1-2-1」 numbering stop
/// being worth reading, so they are only editable in `terminal_sessions.json`.
const MAX_EDITABLE_STEP_DEPTH: usize = 5;

/// What 「タイムアウト時」 offers, `continue_on_timeout` first.
const TIMEOUT_CHOICES: [(bool, &str); 2] = [
    (false, i18n::msg!("Stop the Macro")),
    (true, i18n::msg!("Run the Next Step")),
];

fn timeout_choice_label(continue_on_timeout: bool) -> &'static str {
    TIMEOUT_CHOICES
        .iter()
        .find(|(candidate, _)| *candidate == continue_on_timeout)
        .map(|(_, label)| i18n::tr(label))
        .unwrap_or_default()
}

impl Render for MacroEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.watch_fields(cx);
        self.report_tab_change(cx);
        let defined = self.defined_variables(cx);
        let reach = self.variable_reach(&defined, cx);
        let rem_size = window.rem_size();
        // The tab's width is only known once it has been laid out; before
        // that the window is the most it can have.
        let editor_width = self
            .measured_width
            .get()
            .unwrap_or_else(|| window.viewport_size().width);
        if !self.macro_list_chosen {
            self.show_macro_list = editor_width >= rems(MACRO_LIST_MIN_WIDTH).to_pixels(rem_size);
        }
        // What the cards of the macro's own list get: the column they are
        // laid out in, less the room the form keeps for its scrollbar. Before
        // the column has been laid out, the tab less its padding and the tree
        // beside the form.
        let form_width = match self.measured_form_width.get() {
            Some(width) => width,
            None => {
                let tree_width = if self.show_macro_list {
                    rems(17.).to_pixels(rem_size) + px(12.)
                } else {
                    px(0.)
                };
                editor_width - DynamicSpacing::Base12.rems(cx).to_pixels(rem_size) * 2. - tree_width
            }
        };
        let form_width = (form_width - FORM_SCROLLBAR_ROOM).max(px(0.));
        let steps = self.render_lane(None, form_width, &reach, window, cx);
        let steps_header = self.render_steps_header(&defined, cx);
        let tree = self
            .show_macro_list
            .then(|| self.render_macro_tree(window, cx));
        let listed = self.listed_macros(cx).len();
        let total = self.store.read(cx).macros().len();
        let tree_header = if listed == total {
            i18n::t!("Saved Macros ({total})")
        } else {
            i18n::t!("Saved Macros ({listed} / {total})")
        };
        let form_header = self.form_heading();
        let show_macro_list = self.show_macro_list;
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
                    i18n::t!("Create a folder with this name"),
                    i18n::t!("Cancel creating the folder"),
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

        let measured_width = self.measure_width(&self.measured_width, cx);
        let measured_form_width = self.measure_width(&self.measured_form_width, cx);
        v_flex()
            // The two panes need a bounded box: the tree and the form each
            // scroll inside it rather than growing the tab off screen.
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .key_context("TerminalMacroEditor")
            .debug_selector(|| "macro-editor".into())
            .track_focus(&self.focus_handle)
            .child(measured_width)
            .child(
                v_flex()
                    .size_full()
                    .min_h_0()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .pt_2()
                            .px(DynamicSpacing::Base12.rems(cx))
                            .gap_3()
                            .when_some(tree, |this, tree| this.child(
                                v_flex()
                                    .w(rems(17.))
                                    // A list the user opened in a narrow tab
                                    // leaves the form half of it.
                                    .max_w(relative(0.5))
                                    .debug_selector(|| "macro-list-pane".into())
                                    .flex_none()
                                    .h_full()
                                    .min_h_0()
                                    .gap_1()
                                    .pr_3()
                                    .border_r_1()
                                    .border_color(cx.theme().colors().border)
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
                                                        Button::new(
                                                            "new-macro",
                                                            i18n::tc!("terminal macro", "New"),
                                                        )
                                                            .label_size(LabelSize::Small)
                                                            .start_icon(Icon::new(IconName::Plus))
                                                            .tooltip(Tooltip::text(
                                                                i18n::t!("Opens an empty macro on the right"),
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
                                                        Button::new("new-folder", i18n::t!("Folder"))
                                                            .label_size(LabelSize::Small)
                                                            .start_icon(Icon::new(IconName::Plus))
                                                            .tooltip(Tooltip::text(
                                                                i18n::t!("Creates a folder to group macros"),
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
                            ))
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
                                                h_flex()
                                                    .min_w_0()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .debug_selector(|| {
                                                                "macro-list-toggle".into()
                                                            })
                                                            .child(IconButton::new(
                                                            "macro-list-toggle",
                                                            if show_macro_list {
                                                                IconName::ThreadsSidebarLeftOpen
                                                            } else {
                                                                IconName::ThreadsSidebarLeftClosed
                                                            },
                                                        )
                                                        .icon_size(IconSize::Small)
                                                        .toggle_state(show_macro_list)
                                                        .tooltip(Tooltip::text(
                                                            if show_macro_list {
                                                                i18n::t!("Hides the list of saved macros to widen the editor")
                                                            } else {
                                                                i18n::t!("Shows the list of saved macros")
                                                            },
                                                        ))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.set_macro_list_shown(
                                                                !this.show_macro_list,
                                                                cx,
                                                            );
                                                        }))),
                                                    )
                                                    .child(
                                                        div()
                                                            .min_w_0()
                                                            .debug_selector(|| {
                                                                "macro-form-header".into()
                                                            })
                                                            .child(
                                                                Label::new(form_header)
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Muted)
                                                                    .truncate(),
                                                            ),
                                                    ),
                                            )
                                            .when_some(editing, |this, key| {
                                                this.child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new("duplicate-macro", i18n::t!("Duplicate"))
                                                                .label_size(LabelSize::Small)
                                                                .start_icon(Icon::new(
                                                                    IconName::Copy,
                                                                ))
                                                                .tooltip(Tooltip::text(
                                                                    i18n::t!("Opens a new macro with the same contents"),
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
                                                            Button::new("delete-macro", i18n::t!("Delete"))
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
                                        // The frame is what the column bounds; the form
                                        // inside fills it and scrolls, with a bar that
                                        // shows there is more below.
                                        div()
                                            .id("macro-form-frame")
                                            .debug_selector(|| "macro-form-frame".into())
                                            .relative()
                                            .flex_1()
                                            .min_h_0()
                                            .on_drag_move::<DraggedStep>(cx.listener(
                                                Self::scroll_while_dragging,
                                            ))
                                            .child(measured_form_width)
                                            .child(
                                                v_flex()
                                                    .id("macro-form")
                                                    .debug_selector(|| "macro-form".into())
                                                    .size_full()
                                                    .overflow_y_scroll()
                                                    .track_scroll(&self.form_scroll)
                                                    .gap_2()
                                                    .pr_3()
                                                    .child(name_field)
                                                    .child(
                                                        h_flex()
                                                            .flex_wrap()
                                                            .gap_2()
                                                            .child(
                                                                Label::new(i18n::t!("Folder"))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Muted),
                                                            )
                                                            .child(folder_menu),
                                                    )
                                                    .children(new_folder_in_form)
                                                    .child(tags_field)
                                                    .child(description_field)
                                                    .child(
                                                        // Each label stays with its field when the
                                                        // row wraps in a narrow column.
                                                        h_flex()
                                                            .flex_wrap()
                                                            .gap_2()
                                                            .child(
                                                                h_flex()
                                                                    .flex_none()
                                                                    .gap_2()
                                                                    .child(
                                                                        Label::new(i18n::t!("Repeat Count"))
                                                                            .size(LabelSize::Small)
                                                                            .color(Color::Muted),
                                                                    )
                                                                    .child(repeat_field),
                                                            )
                                                            .child(
                                                                h_flex()
                                                                    .debug_selector(|| "macro-repeat-interval".into())
                                                                    .flex_none()
                                                                    .gap_2()
                                                                    .child(
                                                                        Label::new(i18n::t!("Interval (seconds)"))
                                                                            .size(LabelSize::Small)
                                                                            .color(Color::Muted),
                                                                    )
                                                                    .child(repeat_interval_field),
                                                            )
                                                            .child(
                                                                Label::new(i18n::t!("(0 = repeat until stopped)"))
                                                                    .size(LabelSize::Small)
                                                                    .color(Color::Muted),
                                                            ),
                                                    )

                                                    .child(steps_header)
                                                    .child(steps),
                                            )
                                            .vertical_scrollbar_for(&self.form_scroll, window, cx),
                                    )
                                    .when_some(pending_selection, |this, edited| {
                                        let prompt = match edited
                                            .as_ref()
                                            .filter(|edited| !edited.name.is_empty())
                                        {
                                            Some(edited) => i18n::t!(
                                                "Your changes haven't been saved. Discard them and open “{}”?",
                                                edited.name
                                            ),
                                            None => i18n::t!(
                                                "Your changes haven't been saved. Discard them and open a new macro?"
                                            )
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
                                                                i18n::t!("Discard and Switch"),
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
                                                                i18n::t!("Keep Editing"),
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
                                                    Label::new(i18n::t!(
                                                        "Deleting the folder “{folder}”. What should happen to the macros in it?"
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
                                                                i18n::t!("Move Macros to Unfiled"),
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
                                                                i18n::t!("Delete with Macros"),
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
                                                                i18n::t!("Cancel"),
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
                                                    Label::new(i18n::t!("Delete “{name}”?"))
                                                    .size(LabelSize::Small),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_1()
                                                        .child(
                                                            Button::new("confirm-delete", i18n::t!("Delete"))
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
                                                                i18n::t!("Cancel"),
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
                    .child(
                        h_flex()
                            .w_full()
                            .flex_none()
                            .justify_end()
                            .px(DynamicSpacing::Base12.rems(cx))
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .debug_selector(|| "macro-editor-buttons".into())
                                    .child(
                                        Button::new("save", i18n::t!("Save"))
                                            .style(ButtonStyle::Filled)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.save_from_button(window, cx);
                                            })),
                                    ),
                            ),
                    ),
            )
    }
}

impl MacroEditor {
    /// Tells the pane when the tab's title or its unsaved mark changes. Asked
    /// on every render, since what the form holds changes in the fields'
    /// own editors without passing through this view.
    fn watch_fields(&mut self, cx: &mut Context<Self>) {
        let fields = self.field_editors();
        let current = fields
            .iter()
            .map(|field| field.entity_id())
            .collect::<HashSet<_>>();
        self.field_subscriptions
            .retain(|entity_id, _| current.contains(entity_id));
        for field in fields {
            if self.field_subscriptions.contains_key(&field.entity_id()) {
                continue;
            }
            let subscription = cx.subscribe(&field, |_, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    cx.notify();
                }
            });
            self.field_subscriptions
                .insert(field.entity_id(), subscription);
        }
    }

    fn report_tab_change(&mut self, cx: &mut Context<Self>) {
        let shown = (self.original_key.clone(), self.has_unsaved_edits(cx));
        let conflict = self.macro_changed_elsewhere(cx).is_some() || self.macro_gone_elsewhere(cx);
        if shown != self.reported_tab || conflict != self.reported_conflict {
            self.reported_tab = shown;
            self.reported_conflict = conflict;
            cx.emit(MacroEditorEvent::TabChanged);
        }
    }

    /// An element that paints nothing and records the width its parent was
    /// laid out at in `cell`, asking for another frame when that changed.
    fn measure_width(
        &self,
        cell: &Rc<Cell<Option<Pixels>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let cell = cell.clone();
        let redraw_requested = self.redraw_requested.clone();
        let view = cx.weak_entity();
        canvas(
            move |bounds, window, _| {
                if cell.get() == Some(bounds.size.width) {
                    return;
                }
                cell.set(Some(bounds.size.width));
                if redraw_requested.replace(true) {
                    return;
                }
                // A notify while the frame is being drawn is dropped, so the
                // redraw is asked for from the next frame.
                window.on_next_frame(move |_, cx| {
                    redraw_requested.set(false);
                    // The tab may be closed by then; nothing is left to
                    // redraw.
                    view.update(cx, |_, cx| cx.notify()).ok();
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }

    /// Scrolls the form while a card is dragged near its top or bottom edge,
    /// so a card can be dropped on a 「+」 that is out of view. Holding the
    /// pointer still keeps scrolling; moving it away from the edge, or letting
    /// go, stops.
    fn scroll_while_dragging(
        &mut self,
        event: &DragMoveEvent<DraggedStep>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = event.bounds;
        let position = event.event.position;
        let edge = rems(DRAG_SCROLL_EDGE)
            .to_pixels(window.rem_size())
            .min(bounds.size.height / 4.);
        let inside_horizontally = position.x >= bounds.left() && position.x <= bounds.right();
        let step = if !inside_horizontally {
            None
        } else if position.y < bounds.top() + edge {
            Some(DRAG_SCROLL_STEP)
        } else if position.y > bounds.bottom() - edge {
            Some(-DRAG_SCROLL_STEP)
        } else {
            None
        };
        let Some(step) = step else {
            self.drag_scroll = None;
            return;
        };
        if self.drag_scroll.is_some() {
            // The task already running keeps going; a new one per mouse move
            // would only restart its timer.
            return;
        }
        self.drag_scroll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let keep_going = this
                    .update(cx, |this, cx| {
                        if !cx.has_active_drag() {
                            this.drag_scroll = None;
                            return false;
                        }
                        let offset = this.form_scroll.offset();
                        let max_offset = this.form_scroll.max_offset();
                        let scrolled = (offset.y + step).min(px(0.)).max(-max_offset.y);
                        if scrolled != offset.y {
                            this.form_scroll.set_offset(gpui::point(offset.x, scrolled));
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
                cx.background_executor().timer(DRAG_SCROLL_INTERVAL).await;
            }
        }));
    }
}

impl EventEmitter<MacroEditorEvent> for MacroEditor {}

impl Focusable for MacroEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for MacroEditor {
    type Event = MacroEditorEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.original_key {
            Some((_, name)) => i18n::t!("Macro: {name}").into(),
            None => i18n::t!("New Macro").into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PlayOutlined))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        let (folder, name) = self.original_key.as_ref()?;
        Some(match folder {
            Some(folder) => format!("{folder} / {name}").into(),
            None => name.clone().into(),
        })
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            MacroEditorEvent::TabChanged => f(ItemEvent::UpdateTab),
            MacroEditorEvent::Close => f(ItemEvent::CloseItem),
        }
    }

    fn added_to_workspace(
        &mut self,
        _workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An editor's focus listeners belong to the window they were
        // registered against, so the fields of a tab moved to another window
        // have to register them again.
        for field in self.field_editors() {
            field.update(cx, |editor, cx| {
                editor.rebind_window_focus_listeners(window, cx);
            });
        }
    }

    // Reads only the form's own fields: the workspace asks while it is
    // being updated itself.
    fn is_dirty(&self, cx: &App) -> bool {
        self.has_unsaved_edits(cx)
    }

    fn unsaved_edits_prompt(&self, _cx: &App) -> Option<String> {
        Some(match &self.original_key {
            Some((_, name)) => {
                i18n::t!("The macro \"{name}\" has unsaved edits. Do you want to save it?")
            }
            None => {
                i18n::t!("This new macro has unsaved edits. Do you want to save it?").to_string()
            }
        })
    }

    // A pane only asks before closing a tab that holds no project file when
    // the tab says it stands for one document of its own.
    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn can_save(&self, _cx: &App) -> bool {
        true
    }

    // The pane asks before a save writes over it (「上書き」 then calls
    // `save`, 「編集を破棄」 calls `reload`).
    fn has_conflict(&self, cx: &App) -> bool {
        self.macro_changed_elsewhere(cx).is_some() || self.macro_gone_elsewhere(cx)
    }

    // With `has_conflict`, the pane asks whether to make it again.
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.macro_gone_elsewhere(cx)
    }

    /// 「保存しない」: puts the form back to the macro as saved.
    fn reload(
        &mut self,
        _project: Entity<project::Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let saved = self.original_key.as_ref().and_then(|key| {
            self.store
                .read(cx)
                .macros()
                .iter()
                .find(|candidate| candidate.key() == *key)
                .cloned()
        });
        self.fill_form(saved, window, cx);
        Task::ready(Ok(()))
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        _project: Entity<project::Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.save_form(cx)
    }
}

impl SerializableItem for MacroEditor {
    fn serialized_item_kind() -> &'static str {
        "TerminalMacroEditor"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = persistence::MacroEditorDb::global(cx);
        workspace::delete_unloaded_items(
            alive_items,
            workspace_id,
            "terminal_macro_editors",
            &db,
            cx,
        )
    }

    fn deserialize(
        _project: Entity<project::Project>,
        workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = persistence::MacroEditorDb::global(cx);
        let fs = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().fs.clone());
        window.spawn(cx, async move |cx| {
            let Some(fs) = fs else {
                anyhow::bail!("the workspace is gone, so the macro editor cannot be restored");
            };
            let Some((folder, name)) = db.get_macro_editor(item_id, workspace_id)? else {
                anyhow::bail!("no macro editor was saved for this tab");
            };
            let key = name.map(|name| (folder, name));
            cx.update(|window, cx| {
                let store = TerminalSessionStore::global(fs, cx);
                cx.new(|cx| MacroEditor::restored(store, key, window, cx))
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        // On quitting or closing the window, a tab that serializes is taken as
        // having kept its unsaved edits and is closed without asking. This one
        // keeps only which macro is open, so its edits would be lost without a
        // word; declining here makes the workspace ask 保存 / 保存しない first.
        if closing && self.has_unsaved_edits(cx) {
            return None;
        }
        let workspace_id = workspace.database_id()?;
        // Only which macro is open is kept; what was typed and not saved is
        // not, so a restored tab opens the macro as it was last saved.
        let (folder, name) = match self.original_key.clone() {
            Some((folder, name)) => (folder, Some(name)),
            None => (None, None),
        };
        let db = persistence::MacroEditorDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_macro_editor(item_id, workspace_id, folder, name)
                .await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, MacroEditorEvent::TabChanged)
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub(super) struct MacroEditorDb(ThreadSafeConnection);

    impl Domain for MacroEditorDb {
        const NAME: &str = stringify!(MacroEditorDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE terminal_macro_editors (
                workspace_id INTEGER,
                item_id INTEGER,

                macro_folder TEXT,
                macro_name TEXT,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(MacroEditorDb, [WorkspaceDb]);

    impl MacroEditorDb {
        query! {
            pub(super) async fn save_macro_editor(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                macro_folder: Option<String>,
                macro_name: Option<String>
            ) -> Result<()> {
                INSERT OR REPLACE INTO terminal_macro_editors(item_id, workspace_id, macro_folder, macro_name)
                VALUES (?, ?, ?, ?)
            }
        }

        query! {
            pub(super) fn get_macro_editor(
                item_id: ItemId,
                workspace_id: WorkspaceId
            ) -> Result<Option<(Option<String>, Option<String>)>> {
                SELECT macro_folder, macro_name
                FROM terminal_macro_editors
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

// -------------------------------------------------------------------------
// Quick connect
// -------------------------------------------------------------------------

/// Rejects anything that the shell would treat as more than one word, because
/// the spawned command line is assembled without quoting.
fn validate_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err(i18n::t!("Enter a host name or IP address."));
    }
    if host.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':' | '[' | ']')
    }) {
        Ok(())
    } else {
        Err(i18n::t!(
            "The host name contains characters that aren't allowed."
        ))
    }
}

fn validate_user(user: &str) -> Result<(), &'static str> {
    if user
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'))
    {
        Ok(())
    } else {
        Err(i18n::t!(
            "The user name contains characters that aren't allowed."
        ))
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

        let name = single_line(i18n::t!("Connection name (for saving)"), window, cx);
        let folder = single_line(i18n::t!("Folder (e.g. Sites/HQ; optional)"), window, cx);
        let host = single_line(i18n::t!("Host name / IP address"), window, cx);
        let port = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("22", window, cx);
            editor
        });
        let user = single_line(i18n::t!("User name (SSH)"), window, cx);
        let password = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text(i18n::t!("Password (optional)"), window, cx);
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
                    .map_err(|_| SharedString::from(i18n::t!("Invalid port number.")))?,
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
            self.error = Some(loading_error().into());
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
                    this.error = Some(i18n::t!("Couldn't save: {error}").into());
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
            self.error = Some(loading_error().into());
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
                            log::warn!("failed to prepare askpass: {error:#}");
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
                        workspace.show_error(
                            i18n::t!("The terminal panel is unavailable.").to_string(),
                            cx,
                        )
                    })
                    .ok();
                return;
            };

            let terminal = match spawn.await {
                Ok(terminal) => terminal,
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.show_error(i18n::t!("Couldn't connect: {error}"), cx)
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
                    workspace.show_error(i18n::t!("Couldn't start logging: {error}"), cx);
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
        i18n::t!(
            "telnet was not found. Add “Telnet Client” from Windows “Optional features”, \
             or run `Enable-WindowsOptionalFeature -Online -FeatureName TelnetClient` \
             in PowerShell as an administrator."
        )
        .to_string()
    } else {
        i18n::t!("{program} was not found on PATH.")
    }
}

impl QuickConnectModal {
    fn render_saved_connections(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.store.read(cx).is_loaded() {
            return Label::new(i18n::t!("Loading…"))
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
                            Label::new(i18n::t!("Saved"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                        .child(
                            IconButton::new(("forget-password", index), IconName::LockOff)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(i18n::t!(
                                    "Forgets the saved password of this connection"
                                )))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.forget_password(connection.clone(), cx);
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("delete-connection", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!(
                                "Deletes this connection (and its saved password)"
                            )))
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
            None => i18n::t!("(None)").to_string(),
        };
        let view = cx.weak_entity();
        let macro_menu = ContextMenu::build(window, cx, {
            move |mut menu, _, _| {
                menu = menu.entry(i18n::t!("(None)"), None, {
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
                            .headline(i18n::t!("Quick Connect"))
                            .description(i18n::t!(
                                "Connects to network devices over SSH or Telnet."
                            )),
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
                                            Label::new(i18n::t!("Port"))
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
                                            Label::new(i18n::t!("Macro to Run After Connecting"))
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
                                                .label(i18n::t!("Record a Log"))
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
                                            .label(i18n::t!("Remember password"))
                                            .tooltip(Tooltip::text(i18n::t!(
                                                "The password is kept in the OS credential manager \
                                                 and is never written to terminal_sessions.json"
                                            )))
                                            .on_click(
                                                cx.listener(|this, state: &ToggleState, _, cx| {
                                                    let checked = *state == ToggleState::Selected;
                                                    if this.remember_password != checked {
                                                        this.remember_password = checked;
                                                        cx.notify();
                                                    }
                                                }),
                                            ),
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
                            .header(i18n::t!("Saved Connections").into())
                            .child(saved),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("save-connection", i18n::t!("Save")).on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.save_connection(cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Button::new(
                                            "open-file-transfer",
                                            i18n::t!("Open File Transfer"),
                                        )
                                        .tooltip(Tooltip::text(i18n::t!(
                                            "Transfers files with this host over SFTP / FTP"
                                        )))
                                        .on_click(
                                            cx.listener(|this, _, window, cx| {
                                                this.open_file_transfer(window, cx);
                                            }),
                                        ),
                                    ),
                            )
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Button::new("cancel", i18n::t!("Cancel")).on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(DismissEvent)),
                                        ),
                                    )
                                    .child(Button::new("connect", i18n::t!("Connect")).on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.connect(window, cx);
                                        }),
                                    )),
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

    use gpui::{Modifiers, MouseButton, TestAppContext, VisualTestContext, point, size};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use terminal::terminal_macro::parse_sessions_file;
    use util::path;
    use workspace::{MultiWorkspace, WorkspaceDb};

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
            let editor = cx.new(|cx| MacroEditor::new(store, None, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        })
    }

    /// Lets the editor lay itself out again by the widths it measured: it
    /// reads them during one frame and asks for the next.
    fn settle(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        for _ in 0..4 {
            let callbacks = cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.run_until_parked();
            if callbacks == 0 {
                break;
            }
        }
    }

    fn active_macro_editor(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Option<Entity<MacroEditor>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<MacroEditor>())
        })
    }

    fn macro_editor_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
        workspace.read_with(cx, |workspace, cx| {
            workspace.items_of_type::<MacroEditor>(cx).count()
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
                "Unfiled (1)",
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

        let editor =
            active_macro_editor(&workspace, cx).expect("the editor should open as the active tab");
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.original_key,
                Some((None, "ログイン".to_string())),
                "the editor should have loaded the selected macro"
            );
        });
        workspace.update(cx, |workspace, cx| {
            assert!(
                workspace.active_modal::<MacroPicker>(cx).is_none(),
                "the picker should be gone"
            );
        });
        assert_eq!(macro_editor_count(&workspace, cx), 1);
    }

    #[gpui::test]
    async fn opening_a_macro_already_open_in_a_tab_brings_that_tab_forward(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ログイン", "steps": [] }, { "name": "保存", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let login = stored_macro_in(&store, None, "ログイン", cx);
        let save = stored_macro_in(&store, None, "保存", cx);

        let open = |edited: Option<TerminalMacro>, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                open_macro_editor(workspace, edited, window, cx)
                    .expect("the editor should have opened")
            })
        };
        let first = open(Some(login.clone()), cx);
        let second = open(Some(save), cx);
        cx.run_until_parked();
        assert_ne!(first.entity_id(), second.entity_id());
        assert_eq!(macro_editor_count(&workspace, cx), 2);

        // The same macro again: no second form over it, its tab comes forward.
        let again = open(Some(login), cx);
        cx.run_until_parked();
        assert_eq!(again.entity_id(), first.entity_id());
        assert_eq!(macro_editor_count(&workspace, cx), 2);
        assert_eq!(
            active_macro_editor(&workspace, cx).map(|editor| editor.entity_id()),
            Some(first.entity_id())
        );

        // 「マクロを編集」 with no macro named shows a tab that is open.
        let any = open(None, cx);
        assert_eq!(macro_editor_count(&workspace, cx), 2);
        assert!([first.entity_id(), second.entity_id()].contains(&any.entity_id()));

        // 「新規作成」 always gets a form of its own.
        let fresh = open(Some(TerminalMacro::default()), cx);
        cx.run_until_parked();
        assert_eq!(macro_editor_count(&workspace, cx), 3);
        fresh.read_with(cx, |editor, _| assert_eq!(editor.original_key, None));
        assert_eq!(
            active_macro_editor(&workspace, cx).map(|editor| editor.entity_id()),
            Some(fresh.entity_id())
        );
    }

    #[gpui::test]
    async fn a_macro_whose_tab_was_moved_into_the_terminal_panel_opens_there(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [
                { "name": "ログイン", "steps": [] },
                { "name": "保存", "steps": [] },
                { "name": "終了", "steps": [] }
            ] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let login = stored_macro_in(&store, None, "ログイン", cx);
        let save = stored_macro_in(&store, None, "保存", cx);
        let quit = stored_macro_in(&store, None, "終了", cx);
        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let open = |edited: TerminalMacro, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                open_macro_editor(workspace, Some(edited), window, cx)
                    .expect("the editor should have opened")
            })
        };

        let panel_pane = terminal_panel.read_with(cx, |panel, _| panel.active_pane.clone());
        let move_to_panel = |editor: &Entity<MacroEditor>, cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let center_pane = workspace.active_pane().clone();
                let index = panel_pane.read(cx).items_len();
                workspace::move_item(
                    &center_pane,
                    &panel_pane,
                    editor.entity_id(),
                    index,
                    true,
                    window,
                    cx,
                );
            });
        };
        let moved = open(login.clone(), cx);
        move_to_panel(&moved, cx);
        // Another tab in front of it in the panel, so that finding the tab is
        // not enough: it has to be brought forward.
        let in_front = open(quit, cx);
        move_to_panel(&in_front, cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.close_panel::<TerminalPanel>(window, cx);
        });
        let stays = open(save, cx);
        cx.run_until_parked();
        assert_eq!(macro_editor_count(&workspace, cx), 1);
        assert_eq!(
            panel_pane.read_with(cx, |pane, _| pane.active_item().map(|item| item.item_id())),
            Some(in_front.entity_id())
        );

        let again = open(login, cx);
        cx.run_until_parked();
        assert_eq!(
            again.entity_id(),
            moved.entity_id(),
            "the tab in the terminal panel is the macro's form; no second one opens"
        );
        assert_eq!(macro_editor_count(&workspace, cx), 1);
        let all_tabs = workspace.read_with(cx, |workspace, cx| {
            open_macro_editors(workspace, cx)
                .into_iter()
                .map(|(_, editor)| editor.entity_id())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            all_tabs,
            vec![stays.entity_id(), moved.entity_id(), in_front.entity_id()]
        );
        assert_eq!(
            panel_pane.read_with(cx, |pane, _| pane.active_item().map(|item| item.item_id())),
            Some(moved.entity_id())
        );
        assert!(
            cx.update(|window, cx| moved.focus_handle(cx).contains_focused(window, cx)),
            "the form that was asked for has the focus"
        );
        assert!(
            workspace.read_with(cx, |workspace, cx| workspace.all_docks().iter().any(
                |dock| {
                    let dock = dock.read(cx);
                    dock.panel::<TerminalPanel>().is_some() && dock.is_open()
                }
            )),
            "the terminal panel holding the tab is shown"
        );
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
            editor.save_form(cx).detach();
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

        // The tab stays on the macro just saved, which is now what counts as
        // unchanged and what the tab is named after.
        assert_eq!(
            active_macro_editor(&workspace, cx).map(|active| active.entity_id()),
            Some(editor.entity_id())
        );
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.original_key,
                Some((None, "新しいマクロ".to_string()))
            );
            assert!(!editor.is_dirty(cx));
            assert_eq!(editor.tab_content_text(0, cx), "Macro: 新しいマクロ");
        });

        let picker = open_picker(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(
            picker_names(&picker, cx),
            vec!["新しいマクロ".to_string(), "既存".to_string()]
        );
    }

    fn close_active_tab(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Task<anyhow::Result<()>> {
        let pane = workspace.read_with(cx, |workspace, _| workspace.active_pane().clone());
        pane.update_in(cx, |pane, window, cx| {
            pane.close_active_item(
                &workspace::CloseActiveItem {
                    save_intent: None,
                    close_pinned: false,
                },
                window,
                cx,
            )
        })
    }

    #[gpui::test]
    async fn closing_a_tab_with_unsaved_changes_asks_first(cx: &mut TestAppContext) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "既存", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let existing = stored_macro(&editor, None, "既存", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(existing), window, cx);
        });
        cx.run_until_parked();
        let tab_is_dirty = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .is_some_and(|item| item.is_dirty(cx))
            })
        };
        assert!(!tab_is_dirty(cx));

        // Typing into a field is what makes the tab unsaved; the tab learns
        // it without the form being touched any other way.
        editor.update_in(cx, |editor, window, cx| {
            editor.description.update(cx, |description, cx| {
                description.set_text("書き換えた説明", window, cx);
            });
        });
        cx.run_until_parked();
        assert!(tab_is_dirty(cx));
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.reported_tab,
                (Some((None, "既存".to_string())), true),
                "the pane should have been told that the tab is unsaved"
            );
        });

        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        let (message, _) = cx.pending_prompt().expect("closing should ask what to do");
        assert!(
            message.contains("既存"),
            "the question should name the macro: {message}"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        close.await.log_err();
        assert_eq!(
            macro_editor_count(&workspace, cx),
            1,
            "Cancel keeps the tab"
        );

        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
        close.await.expect("saving and closing should succeed");
        assert_eq!(macro_editor_count(&workspace, cx), 0, "Save closes the tab");
        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        assert_eq!(
            parse_sessions_file(&contents).macros[0].description,
            "書き換えた説明"
        );

        // A tab with nothing unsaved closes without asking.
        let editor = open_editor(&workspace, &store, cx);
        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| assert!(!editor.is_dirty(cx)));
        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        close.await.expect("closing a clean tab should succeed");
        assert_eq!(macro_editor_count(&workspace, cx), 0);
    }

    #[gpui::test]
    async fn saving_a_form_that_cannot_be_saved_keeps_the_tab_open(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.add_step(StepKind::Send, window, cx);
        });
        cx.run_until_parked();

        // No name: 「保存」 in the close prompt is refused, the pane says why
        // and offers to stay, and staying leaves the form as it was.
        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "a refused save should be reported before the tab closes"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        close.await.expect("staying open is not an error");
        assert_eq!(macro_editor_count(&workspace, cx), 1);
        editor.read_with(cx, |editor, cx| {
            assert!(editor.is_dirty(cx));
            assert_eq!(editor.steps.len(), 1);
        });
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_ref().map(|error| error.to_string()),
                Some("Enter a macro name.".to_string())
            );
        });
    }

    #[gpui::test]
    async fn a_restored_tab_reopens_its_macro_once_the_file_is_read(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "folder": "A", "name": "ls", "steps": [{ "type": "clear" }] },
                            { "name": "ls", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        let focused = workspace.update_in(cx, |workspace, window, cx| {
            let focused = cx.new(|cx| workspace::item::test::TestItem::new(cx));
            workspace.add_item_to_active_pane(Box::new(focused.clone()), None, true, window, cx);
            focused
        });
        // Restored tabs come back beside whatever has the focus.
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let key = Some((Some("A".to_string()), "ls".to_string()));
            let editor = cx.new(|cx| MacroEditor::restored(store.clone(), key, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, false, window, cx);
            workspace.activate_item(&focused, true, true, window, cx);
            editor
        });
        editor.read_with(cx, |editor, cx| {
            assert!(!store.read(cx).is_loaded());
            assert_eq!(editor.original_key, None, "nothing to open before the read");
        });

        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.original_key,
                Some((Some("A".to_string()), "ls".to_string())),
                "the macro is found by its folder as well as its name"
            );
            assert_eq!(editor.steps.len(), 1);
            assert!(
                !editor.is_dirty(cx),
                "a restored tab opens the macro as saved"
            );
        });
        cx.update(|window, cx| {
            assert!(
                focused.focus_handle(cx).is_focused(window),
                "a tab restored in the background does not take the focus"
            );
        });
    }

    #[gpui::test]
    async fn typing_into_a_restored_tab_before_the_file_is_read_is_kept(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ls", "description": "保存済み", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            let key = Some((None, "ls".to_string()));
            let editor = cx.new(|cx| MacroEditor::restored(store.clone(), key, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor
        });
        editor.update_in(cx, |editor, window, cx| {
            assert!(!editor.store.read(cx).is_loaded());
            editor.description.update(cx, |description, cx| {
                description.set_text("読み込み中に入力", window, cx);
            });
        });
        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| {
            assert!(editor.store.read(cx).is_loaded());
            assert_eq!(editor.description.read(cx).text(cx), "読み込み中に入力");
            assert_eq!(editor.original_key, None);
            assert!(editor.is_dirty(cx));
        });
    }

    #[gpui::test]
    async fn a_macro_changed_elsewhere_is_not_written_over_without_asking(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ログイン", "description": "元", "steps": [] },
                            { "name": "別", "steps": [{ "type": "clear" }] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let login = stored_macro_in(&store, None, "ログイン", cx);
        // The same macro in two tabs: the list of each tab opens any macro,
        // whatever the other tabs show.
        let first = open_editor(&workspace, &store, cx);
        let second = open_editor(&workspace, &store, cx);
        for editor in [&first, &second] {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(login.clone()), window, cx);
            });
        }
        cx.run_until_parked();
        let description = |editor: &Entity<MacroEditor>, cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, cx| editor.description.read(cx).text(cx))
        };
        let set_description =
            |editor: &Entity<MacroEditor>, text: &str, cx: &mut VisualTestContext| {
                editor.update_in(cx, |editor, window, cx| {
                    editor.description.update(cx, |description, cx| {
                        description.set_text(text, window, cx);
                    });
                });
            };
        async fn saved_description(fs: &Arc<FakeFs>) -> Option<String> {
            saved_file(fs)
                .await
                .macros
                .iter()
                .find(|terminal_macro| terminal_macro.name == "ログイン")
                .map(|terminal_macro| terminal_macro.description.clone())
        }

        // A tab with nothing of its own unsaved shows what the other saved.
        set_description(&first, "一つ目", cx);
        first.update(cx, |editor, cx| editor.save_form(cx).detach());
        cx.run_until_parked();
        assert_eq!(description(&second, cx), "一つ目");
        second.read_with(cx, |editor, cx| {
            assert!(!editor.is_dirty(cx));
            assert!(!editor.has_conflict(cx));
        });

        // A tab with edits of its own keeps them, and is marked.
        set_description(&second, "二つ目", cx);
        set_description(&first, "一つ目の再変更", cx);
        first.update(cx, |editor, cx| editor.save_form(cx).detach());
        cx.run_until_parked();
        assert_eq!(description(&second, cx), "二つ目");
        second.read_with(cx, |editor, cx| {
            assert!(editor.is_dirty(cx));
            assert!(editor.has_conflict(cx));
        });
        first.read_with(cx, |editor, cx| assert!(!editor.has_conflict(cx)));

        // 「保存」 asks before writing over the other tab's save.
        second.update_in(cx, |editor, window, cx| editor.save_from_button(window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "saving over a change should ask");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(
            saved_description(&fs).await.as_deref(),
            Some("一つ目の再変更")
        );

        second.update_in(cx, |editor, window, cx| editor.save_from_button(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Overwrite");
        cx.run_until_parked();
        assert_eq!(saved_description(&fs).await.as_deref(), Some("二つ目"));
        second.read_with(cx, |editor, cx| {
            assert!(!editor.is_dirty(cx));
            assert!(!editor.has_conflict(cx));
        });
        assert_eq!(description(&first, cx), "二つ目");
        assert_eq!(
            saved_file(&fs).await.macros[1].steps,
            vec![MacroStep::Clear {}],
            "the other macro in the file is left as it was"
        );

        // A hand edit of the file reaches a tab with nothing unsaved.
        fs.insert_file(
            sessions_path(),
            r#"{ "macros": [{ "name": "ログイン", "description": "手で直した", "steps": [] }] }"#
                .as_bytes()
                .to_vec(),
        )
        .await;
        cx.run_until_parked();
        assert_eq!(description(&first, cx), "手で直した");
        assert_eq!(description(&second, cx), "手で直した");

        // Saving through the pane (Ctrl+S) asks the same, and 「編集を破棄」
        // puts the form back to what the file holds.
        set_description(&second, "破棄する", cx);
        fs.insert_file(
            sessions_path(),
            r#"{ "macros": [{ "name": "ログイン", "description": "外の変更", "steps": [] }] }"#
                .as_bytes()
                .to_vec(),
        )
        .await;
        cx.run_until_parked();
        let save = workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&second, true, true, window, cx);
            workspace.save_active_item(workspace::SaveIntent::Save, window, cx)
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Discard Edits");
        cx.run_until_parked();
        save.await.expect("discarding should succeed");
        assert_eq!(description(&second, cx), "外の変更");
        second.read_with(cx, |editor, cx| assert!(!editor.is_dirty(cx)));
        assert_eq!(saved_description(&fs).await.as_deref(), Some("外の変更"));

        // Saving from the close prompt goes through the pane too.
        set_description(&first, "閉じる前", cx);
        fs.insert_file(
            sessions_path(),
            r#"{ "macros": [{ "name": "ログイン", "description": "また手で", "steps": [] }] }"#
                .as_bytes()
                .to_vec(),
        )
        .await;
        cx.run_until_parked();
        first.read_with(cx, |editor, cx| assert!(editor.has_conflict(cx)));
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&first, true, true, window, cx);
        });
        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Discard Edits");
        cx.run_until_parked();
        close
            .await
            .expect("closing after discarding should succeed");
        assert_eq!(saved_description(&fs).await.as_deref(), Some("また手で"));
    }

    #[gpui::test]
    async fn a_macro_gone_elsewhere_closes_its_tab_unless_the_tab_has_edits(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "消す1", "steps": [] },
                            { "name": "消す2", "steps": [] },
                            { "folder": "F", "name": "移す1", "steps": [] },
                            { "folder": "F", "name": "移す2", "steps": [] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let open = |folder: Option<&str>, name: &str, cx: &mut VisualTestContext| {
            let terminal_macro = stored_macro_in(&store, folder, name, cx);
            let editor = open_editor(&workspace, &store, cx);
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(terminal_macro), window, cx);
            });
            editor
        };
        let edit = |editor: &Entity<MacroEditor>, cx: &mut VisualTestContext| {
            editor.update_in(cx, |editor, window, cx| {
                editor.description.update(cx, |description, cx| {
                    description.set_text("未保存", window, cx);
                });
            });
        };
        let clean_deleted = open(None, "消す1", cx);
        let edited_deleted = open(None, "消す2", cx);
        edit(&edited_deleted, cx);
        let clean_moved = open(Some("F"), "移す1", cx);
        let edited_moved = open(Some("F"), "移す2", cx);
        edit(&edited_moved, cx);
        // A form not saved yet stands for nothing in the file to go missing.
        let blank = open_editor(&workspace, &store, cx);
        cx.run_until_parked();
        assert_eq!(macro_editor_count(&workspace, cx), 5);

        // Deleted by hand, and the folder renamed.
        fs.insert_file(
            sessions_path(),
            r#"{ "macros": [{ "folder": "G", "name": "移す1", "steps": [] },
                            { "folder": "G", "name": "移す2", "steps": [] }] }"#
                .as_bytes()
                .to_vec(),
        )
        .await;
        cx.run_until_parked();
        let open_ids = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<MacroEditor>(cx)
                .map(|editor| editor.entity_id())
                .collect::<HashSet<_>>()
        });
        assert_eq!(
            open_ids,
            HashSet::from_iter([
                edited_deleted.entity_id(),
                edited_moved.entity_id(),
                blank.entity_id()
            ]),
            "the tabs with nothing unsaved over a macro that is gone close"
        );
        assert!(!open_ids.contains(&clean_deleted.entity_id()));
        assert!(!open_ids.contains(&clean_moved.entity_id()));
        for editor in [&edited_deleted, &edited_moved] {
            editor.read_with(cx, |editor, cx| {
                assert_eq!(editor.description.read(cx).text(cx), "未保存");
                assert!(editor.has_conflict(cx));
                assert!(editor.has_deleted_file(cx));
            });
        }
        blank.read_with(cx, |editor, cx| assert!(!editor.has_conflict(cx)));

        // 「保存」 says the macro will be made again before doing it.
        edited_deleted.update_in(cx, |editor, window, cx| editor.save_from_button(window, cx));
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(saved_file(&fs).await.macro_named(None, "消す2").is_none());
        edited_deleted.update_in(cx, |editor, window, cx| editor.save_from_button(window, cx));
        cx.run_until_parked();
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
        assert_eq!(
            saved_file(&fs)
                .await
                .macro_named(None, "消す2")
                .map(|terminal_macro| terminal_macro.description.clone())
                .as_deref(),
            Some("未保存")
        );
        edited_deleted.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert!(!editor.is_dirty(cx));
        });

        // Closing asks too (the pane's own question for a file that is gone).
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&edited_moved, true, true, window, cx);
        });
        let close = close_active_tab(&workspace, cx);
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        close.await.log_err();
        assert!(
            saved_file(&fs)
                .await
                .macro_named(Some("F"), "移す2")
                .is_none(),
            "cancelling leaves the macro where the other change put it"
        );
        assert_eq!(macro_editor_count(&workspace, cx), 3);

        // A rename saved from the tab itself is not taken for one made
        // elsewhere.
        let renamed = open(Some("G"), "移す1", cx);
        renamed.update_in(cx, |editor, window, cx| {
            editor
                .name
                .update(cx, |name, cx| name.set_text("移した", window, cx));
            editor.save_form(cx).detach();
        });
        cx.run_until_parked();
        assert!(
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .items_of_type::<MacroEditor>(cx)
                    .any(|editor| editor.entity_id() == renamed.entity_id())
            }),
            "the tab that renamed its own macro stays open"
        );
        renamed.read_with(cx, |editor, cx| {
            assert!(!editor.has_conflict(cx));
            assert_eq!(
                editor.original_key,
                Some((Some("G".to_string()), "移した".to_string()))
            );
        });
    }

    #[gpui::test]
    async fn coming_back_to_the_tab_focuses_the_field_it_was_left_from(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.add_step(StepKind::Send, window, cx);
            editor.add_step(StepKind::Send, window, cx);
        });
        cx.run_until_parked();
        let other = workspace.update_in(cx, |workspace, window, cx| {
            let other = cx.new(|cx| workspace::item::test::TestItem::new(cx));
            workspace.add_item_to_active_pane(Box::new(other.clone()), None, false, window, cx);
            other
        });
        let field_focused = |field: &dyn Fn(&MacroEditor) -> Entity<Editor>,
                             cx: &mut VisualTestContext| {
            cx.update(|window, cx| field(editor.read(cx)).focus_handle(cx).is_focused(window))
        };
        let leave_and_return = |cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_item(&other, true, true, window, cx);
            });
            cx.run_until_parked();
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.activate_item(&editor, true, true, window, cx);
            });
            cx.run_until_parked();
        };

        let second_text = |editor: &MacroEditor| editor.steps[1].text.clone();
        editor.update_in(cx, |editor, window, cx| {
            window.focus(&editor.steps[1].text.focus_handle(cx), cx);
        });
        cx.run_until_parked();
        assert!(field_focused(&second_text, cx));
        leave_and_return(cx);
        assert!(
            field_focused(&second_text, cx),
            "the step field the tab was left from has the focus again"
        );

        // A field that has left the form while the tab was away gives way to
        // the name.
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&other, true, true, window, cx);
        });
        cx.run_until_parked();
        editor.update(cx, |editor, cx| {
            editor.steps.truncate(1);
            cx.notify();
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.activate_item(&editor, true, true, window, cx);
        });
        cx.run_until_parked();
        assert!(field_focused(
            &|editor: &MacroEditor| editor.name.clone(),
            cx
        ));
    }

    #[gpui::test]
    async fn quitting_asks_about_unsaved_edits_rather_than_keeping_only_the_tab(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) =
            init_workspace(cx, r#"{ "macros": [{ "name": "既存", "steps": [] }] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let editor = open_editor(&workspace, &store, cx);
        let existing = stored_macro(&editor, None, "既存", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(existing), window, cx);
        });
        workspace.update(cx, |workspace, _| workspace.set_random_database_id());
        let item_id = editor.entity_id().as_u64() as ItemId;
        // Whether the workspace may take the tab as kept on quitting: it only
        // asks about the tabs that return `None`.
        let serializes = |closing: bool, cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, cx| {
                editor.update(cx, |editor, cx| {
                    editor.serialize(workspace, item_id, closing, cx).is_some()
                })
            })
        };
        assert!(serializes(true, cx), "nothing unsaved: the tab is kept");

        editor.update_in(cx, |editor, window, cx| {
            editor.description.update(cx, |description, cx| {
                description.set_text("未保存", window, cx);
            });
        });
        assert!(
            !serializes(true, cx),
            "the unsaved edits are not kept, so quitting has to ask about them"
        );
        assert!(serializes(false, cx), "which macro is open is still kept");
    }

    #[gpui::test]
    async fn the_database_brings_back_the_macro_a_tab_had_open(cx: &mut TestAppContext) {
        let (_fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
        // What a restored tab reads: the app's own sessions file.
        let store = cx.update(|_, cx| TerminalSessionStore::global(fs, cx));
        cx.run_until_parked();
        store
            .update(cx, |store, cx| {
                let mut file = store.file().clone();
                file.macros.push(TerminalMacro {
                    folder: Some("A".to_string()),
                    name: "dir".to_string(),
                    steps: vec![MacroStep::Clear {}],
                    ..TerminalMacro::default()
                });
                store.write(file, cx)
            })
            .await
            .expect("the sessions file should be written");
        cx.run_until_parked();

        let workspace_db = cx.update(|_, cx| WorkspaceDb::global(cx));
        let workspace_id = workspace_db
            .next_id()
            .await
            .expect("a workspace id should be handed out");
        let db = cx.update(|_, cx| persistence::MacroEditorDb::global(cx));
        db.save_macro_editor(
            1,
            workspace_id,
            Some("A".to_string()),
            Some("ls".to_string()),
        )
        .await
        .expect("the tab should be saved");
        // Saving the same tab again replaces its row.
        db.save_macro_editor(
            1,
            workspace_id,
            Some("A".to_string()),
            Some("dir".to_string()),
        )
        .await
        .expect("the tab should be saved again");
        db.save_macro_editor(2, workspace_id, None, None)
            .await
            .expect("a new-macro tab should be saved");
        assert_eq!(
            db.get_macro_editor(1, workspace_id)
                .expect("the row should be read"),
            Some((Some("A".to_string()), Some("dir".to_string())))
        );
        assert_eq!(
            db.get_macro_editor(2, workspace_id)
                .expect("the row should be read"),
            Some((None, None))
        );
        assert_eq!(
            db.get_macro_editor(3, workspace_id)
                .expect("no row is not an error"),
            None
        );

        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
        let deserialize = |item_id: ItemId, cx: &mut VisualTestContext| {
            let project = project.clone();
            let workspace = workspace.downgrade();
            cx.update(|window, cx| {
                <MacroEditor as SerializableItem>::deserialize(
                    project,
                    workspace,
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
        };
        let restored = deserialize(1, cx)
            .await
            .expect("the tab should be restored");
        cx.run_until_parked();
        restored.read_with(cx, |editor, cx| {
            assert_eq!(
                editor.original_key,
                Some((Some("A".to_string()), "dir".to_string()))
            );
            assert_eq!(editor.steps.len(), 1);
            assert!(!editor.is_dirty(cx));
        });
        let blank = deserialize(2, cx)
            .await
            .expect("a new-macro tab should be restored");
        blank.read_with(cx, |editor, _| assert_eq!(editor.original_key, None));
        assert!(
            deserialize(3, cx).await.is_err(),
            "a tab that saved nothing is not made up"
        );

        // Closed tabs do not stay in the table.
        cx.update(|window, cx| {
            <MacroEditor as SerializableItem>::cleanup(workspace_id, vec![1], window, cx)
        })
        .await
        .expect("the closed tabs should be cleaned up");
        assert_eq!(
            db.get_macro_editor(2, workspace_id)
                .expect("the row should be read"),
            None
        );
        assert!(
            db.get_macro_editor(1, workspace_id)
                .expect("the row should be read")
                .is_some()
        );
    }

    #[gpui::test]
    async fn holding_a_dragged_card_at_the_edge_of_the_form_scrolls_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(long_macro()), window, cx);
        });
        cx.run_until_parked();
        let offset = |cx: &mut VisualTestContext| {
            editor.read_with(cx, |editor, _| editor.form_scroll.offset().y)
        };
        assert_eq!(offset(cx), px(0.));

        let header = cx
            .debug_bounds("macro-step-1-header")
            .expect("the first card should be on screen");
        let frame = cx
            .debug_bounds("macro-form-frame")
            .expect("the form should have been drawn");
        cx.simulate_mouse_down(header.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            header.center() + point(px(20.), px(0.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        let near_bottom = point(frame.center().x, frame.bottom() - px(4.));
        cx.simulate_mouse_move(near_bottom, MouseButton::Left, Modifiers::none());
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        let scrolled = offset(cx);
        assert!(
            scrolled < px(0.),
            "holding the card at the bottom edge should scroll the form down"
        );

        // Away from the edges it stops, and stays where it got to.
        cx.simulate_mouse_move(frame.center(), MouseButton::Left, Modifiers::none());
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        let stopped = offset(cx);
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert_eq!(offset(cx), stopped);

        // At the top edge it comes back up, no further than the top.
        let near_top = point(frame.center().x, frame.top() + px(4.));
        cx.simulate_mouse_move(near_top, MouseButton::Left, Modifiers::none());
        cx.executor().advance_clock(Duration::from_secs(30));
        cx.run_until_parked();
        assert_eq!(offset(cx), px(0.));

        // Letting go ends it, even at the edge.
        cx.simulate_mouse_move(near_bottom, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(near_bottom, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
        let dropped = offset(cx);
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert_eq!(offset(cx), dropped);
        editor.read_with(cx, |editor, _| assert!(editor.drag_scroll.is_none()));
    }

    #[gpui::test]
    async fn every_level_of_a_nested_repeat_is_editable(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let twice_nested = MacroStep::Repeat {
            count: 2,
            steps: vec![MacroStep::Clear {}],
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
            editor.save_form(cx).detach();
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
                        steps: vec![MacroStep::Clear {}, MacroStep::Sleep { ms: 1_000 }],
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
                            on_timeout: Some(vec![MacroStep::Clear {}]),
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
            assert_eq!(branch.scope, ScopeChoice::Screen);
            assert_eq!(branch.capture.read(cx).text(cx), "reason");
            assert_eq!(branch.children.len(), 1, "“Then” keeps its repeat");
            assert_eq!(branch.else_children.len(), 1, "“Else” keeps its send");
            let wait = &branch.children[0].children[0];
            assert_eq!(wait.capture.read(cx).text(cx), "banner");
            assert!(wait.continue_on_timeout);
            assert_eq!(wait.timeout_children.len(), 1);
        });

        editor.update_in(cx, |editor, _, cx| editor.save_form(cx).detach());
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
                        on_timeout: Some(vec![MacroStep::Clear {}]),
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
            editor.save_form(cx).detach();
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
    async fn a_long_macro_scrolls_inside_the_form(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(long_macro()), window, cx);
        });
        cx.run_until_parked();

        let tab = cx
            .debug_bounds("macro-editor")
            .expect("the editor should have been drawn");
        let form = cx
            .debug_bounds("macro-form")
            .expect("the form should have been drawn");
        let buttons = cx
            .debug_bounds("macro-editor-buttons")
            .expect("the Save button should have been drawn");
        let window_height = cx.update(|window, _| window.viewport_size().height);
        assert!(
            tab.bottom() <= window_height,
            "the tab ends at {} in a window {window_height} high",
            tab.bottom(),
        );
        assert!(
            form.bottom() <= tab.bottom(),
            "the form ends at {} but the tab at {}, so its tail is clipped instead of scrolled",
            form.bottom(),
            tab.bottom()
        );
        assert!(
            buttons.bottom() <= tab.bottom(),
            "Save ends at {} but the tab at {}, so it cannot be clicked",
            buttons.bottom(),
            tab.bottom()
        );
    }

    fn long_macro() -> TerminalMacro {
        TerminalMacro {
            name: "長い".to_string(),
            steps: (0..60)
                .map(|index| MacroStep::Send {
                    text: format!("echo {index}"),
                    newline: true,
                })
                .collect(),
            ..TerminalMacro::default()
        }
    }

    #[gpui::test]
    async fn adding_a_step_to_a_long_macro_brings_it_into_view(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(long_macro()), window, cx);
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            editor.add_step(StepKind::If, window, cx);
        });
        cx.run_until_parked();

        let form = cx
            .debug_bounds("macro-form")
            .expect("the form should have been drawn");
        let added = cx
            .debug_bounds("macro-step-61")
            .expect("the added step should have been drawn");
        assert!(
            added.top() >= form.top() && added.bottom() <= form.bottom(),
            "the added step spans {}..{} but the form shows {}..{}",
            added.top(),
            added.bottom(),
            form.top(),
            form.bottom()
        );
        editor.update_in(cx, |editor, window, cx| {
            assert!(
                editor.steps[60]
                    .text
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                "the new branch should be ready to type its text to match into"
            );
        });
    }

    #[gpui::test]
    async fn opening_another_macro_starts_at_the_top_of_the_form(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(long_macro()), window, cx);
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            editor.add_step(StepKind::Send, window, cx);
        });
        cx.run_until_parked();
        let form = cx
            .debug_bounds("macro-form")
            .expect("the form should have been drawn");
        let first = cx
            .debug_bounds("macro-step-1")
            .expect("the first step should have been drawn");
        assert!(
            first.bottom() < form.top(),
            "adding a step should have scrolled the first one out of view"
        );

        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(long_macro()), window, cx);
        });
        cx.run_until_parked();
        let form = cx
            .debug_bounds("macro-form")
            .expect("the form should have been drawn");
        let first = cx
            .debug_bounds("macro-step-1")
            .expect("the first step should have been drawn");
        assert!(
            first.top() >= form.top() && first.bottom() <= form.bottom(),
            "the first step spans {}..{} but the form shows {}..{}",
            first.top(),
            first.bottom(),
            form.top(),
            form.bottom()
        );
    }

    #[gpui::test]
    async fn a_regex_that_uses_a_variable_can_be_saved(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("ping", window, cx);
            });
            editor.add_step(StepKind::WaitFor, window, cx);
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text(r"^From ${host}: bytes", window, cx);
            });
            editor.steps[0].regex = true;
            editor.save_form(cx).detach();
        });
        cx.run_until_parked();

        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.error, None);
        });
        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved.macros[0].steps,
            vec![MacroStep::WaitFor {
                pattern: r"^From ${host}: bytes".to_string(),
                regex: true,
                timeout_ms: terminal::terminal_macro::DEFAULT_WAIT_TIMEOUT_MS,
                capture: None,
                on_timeout: None,
            }]
        );
    }

    #[gpui::test]
    async fn a_set_variable_and_a_branch_on_it_are_saved(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("変数", window, cx);
            });
            editor.add_step(StepKind::SetVariable, window, cx);
            editor.steps[0].capture.update(cx, |capture, cx| {
                capture.set_text("host", window, cx);
            });
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("rt01", window, cx);
            });
            editor.add_step(StepKind::If, window, cx);
            editor.steps[1].text.update(cx, |text, cx| {
                text.set_text("^rt", window, cx);
            });
            editor.steps[1].regex = true;
            editor.steps[1].scope = ScopeChoice::Variable;
            // Written the way the name is used everywhere else in the form.
            editor.steps[1].scope_variable.update(cx, |variable, cx| {
                variable.set_text("${host}", window, cx);
            });
            editor.save_form(cx).detach();
        });
        cx.run_until_parked();

        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.error, None);
        });
        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        let saved = parse_sessions_file(&contents);
        assert_eq!(
            saved.macros[0].steps,
            vec![
                MacroStep::SetVariable {
                    name: "host".to_string(),
                    value: "rt01".to_string(),
                },
                MacroStep::If {
                    pattern: "^rt".to_string(),
                    regex: true,
                    scope: MatchScope::Variable("host".to_string()),
                    capture: None,
                    then_steps: Vec::new(),
                    else_steps: Vec::new(),
                },
            ]
        );
    }

    #[gpui::test]
    async fn a_variable_step_or_branch_needs_a_name(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("名前なし", window, cx);
            });
            editor.add_step(StepKind::SetVariable, window, cx);
            editor.save_form(cx).detach();
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 1: enter a variable name.")
            );

            editor.steps[0].capture.update(cx, |capture, cx| {
                capture.set_text("host", window, cx);
            });
            editor.add_step(StepKind::If, window, cx);
            editor.steps[1].text.update(cx, |text, cx| {
                text.set_text("rt", window, cx);
            });
            editor.steps[1].scope = ScopeChoice::Variable;
            editor.save_form(cx).detach();
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 2: enter the variable to check.")
            );
        });
    }

    #[gpui::test]
    async fn the_form_lists_its_variables_and_flags_unknown_references(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            let terminal_macro = TerminalMacro {
                name: "変数".to_string(),
                steps: vec![
                    MacroStep::SetVariable {
                        name: "host".to_string(),
                        value: "rt01".to_string(),
                    },
                    MacroStep::If {
                        pattern: r"Version (?<ver>\S+)".to_string(),
                        regex: true,
                        scope: MatchScope::Screen,
                        capture: Some("banner".to_string()),
                        then_steps: vec![MacroStep::SetVariable {
                            name: "host".to_string(),
                            value: "again".to_string(),
                        }],
                        else_steps: vec![MacroStep::SetVariable {
                            name: "fallback".to_string(),
                            value: String::new(),
                        }],
                    },
                    MacroStep::Send {
                        text: "ping ${host} ${ver} ${typo} $${literal}".to_string(),
                        newline: true,
                    },
                ],
                ..TerminalMacro::default()
            };
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        cx.run_until_parked();

        editor.read_with(cx, |editor, cx| {
            let defined = editor
                .defined_variables(cx)
                .into_iter()
                .map(|(name, path)| (name, path.label()))
                .collect::<Vec<_>>();
            assert_eq!(
                defined,
                vec![
                    ("host".to_string(), "1".to_string()),
                    ("banner".to_string(), "2".to_string()),
                    ("ver".to_string(), "2".to_string()),
                    ("fallback".to_string(), "2-e1".to_string()),
                ]
            );
            let reach = editor.variable_reach(&editor.defined_variables(cx), cx);
            assert_eq!(
                editor.steps[2].undefined_references(&reach, cx),
                vec![Unset {
                    name: "typo".to_string(),
                    set_later: false,
                }]
            );
            assert_eq!(
                editor.steps[1].describe(cx).as_deref(),
                Some(
                    "Runs “Then” if the whole screen matches the regular expression “Version (?<ver>\\S+)”, and “Else” if not. Checks right away without waiting. On a match, puts the matched part into ${banner}."
                )
            );
        });

        // Switching the 分岐 to another kind takes its lists out of the macro,
        // and the variables they set with them.
        editor.update(cx, |editor, cx| {
            editor.steps[1].kind = StepKind::Send;
            cx.notify();
        });
        editor.read_with(cx, |editor, cx| {
            let names = editor
                .defined_variables(cx)
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>();
            assert_eq!(names, vec!["host".to_string()]);
        });

        assert!(cx.debug_bounds("macro-help-panel").is_none());
        editor.update(cx, |editor, cx| {
            editor.show_help = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("macro-help-panel").is_some(),
            "“How Branches and Variables Work” should open its panel"
        );
    }

    #[gpui::test]
    async fn a_branch_on_a_variable_nothing_sets_is_flagged(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let branch_on = |scope: MatchScope| MacroStep::If {
            pattern: "rt".to_string(),
            regex: false,
            scope,
            capture: None,
            then_steps: Vec::new(),
            else_steps: Vec::new(),
        };
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            let terminal_macro = TerminalMacro {
                name: "綴り".to_string(),
                steps: vec![
                    MacroStep::SetVariable {
                        name: "host".to_string(),
                        value: "rt01".to_string(),
                    },
                    branch_on(MatchScope::Variable("hots".to_string())),
                    branch_on(MatchScope::Variable("host".to_string())),
                    branch_on(MatchScope::Screen),
                ],
                ..TerminalMacro::default()
            };
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        cx.run_until_parked();

        editor.update_in(cx, |editor, window, cx| {
            // A 対象 switched away from 変数 keeps the name it had, which must
            // not be taken for a variable the branch reads.
            editor.steps[3].scope_variable.update(cx, |variable, cx| {
                variable.set_text("hots", window, cx);
            });
            let reach = editor.variable_reach(&editor.defined_variables(cx), cx);
            assert_eq!(
                editor.steps[1].undefined_scope_variable(&reach, cx),
                Some(Unset {
                    name: "hots".to_string(),
                    set_later: false,
                }),
                "a misspelt variable to check would take “Else” every time without a word"
            );
            assert_eq!(editor.steps[2].undefined_scope_variable(&reach, cx), None);
            assert_eq!(editor.steps[3].undefined_scope_variable(&reach, cx), None);
        });
    }

    #[gpui::test]
    async fn a_reference_is_flagged_by_what_the_steps_before_it_set(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let send = |text: &str| MacroStep::Send {
            text: text.to_string(),
            newline: true,
        };
        let set = |name: &str| MacroStep::SetVariable {
            name: name.to_string(),
            value: "v".to_string(),
        };
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            let terminal_macro = TerminalMacro {
                name: "順序".to_string(),
                steps: vec![
                    // 1: `late` is only set by step 5.
                    send("show ${late}"),
                    // 2: `branch_then` is set on the 「そのとき」 side only,
                    // `own` by the branch's own capture.
                    MacroStep::If {
                        pattern: "${own}".to_string(),
                        regex: false,
                        scope: MatchScope::Variable("late".to_string()),
                        capture: Some("own".to_string()),
                        then_steps: vec![send("${own}"), set("branch_then")],
                        else_steps: vec![send("${own}")],
                    },
                    // 3: whatever either side may set counts from here on.
                    send("${branch_then} ${own}"),
                    // 4: a repeat body sees what its later steps set on the
                    // pass before; a body that runs once does not.
                    MacroStep::Repeat {
                        count: 3,
                        steps: vec![send("${looped}"), set("looped")],
                    },
                    MacroStep::Repeat {
                        count: 1,
                        steps: vec![send("${once}"), set("once")],
                    },
                    set("late"),
                ],
                ..TerminalMacro::default()
            };
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        cx.run_until_parked();

        editor.read_with(cx, |editor, cx| {
            let reach = editor.variable_reach(&editor.defined_variables(cx), cx);
            let unset = |row: &StepRow| {
                row.undefined_references(&reach, cx)
                    .into_iter()
                    .map(|unset| (unset.name, unset.set_later))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                unset(&editor.steps[0]),
                vec![("late".to_string(), true)],
                "a value a later step sets is not there yet"
            );
            assert_eq!(
                editor.steps[1].undefined_scope_variable(&reach, cx),
                Some(Unset {
                    name: "late".to_string(),
                    set_later: true,
                })
            );
            assert_eq!(
                unset(&editor.steps[1]),
                vec![("own".to_string(), true)],
                "the branch's pattern is read before its own capture is stored"
            );
            assert_eq!(unset(&editor.steps[1].children[0]), Vec::new());
            assert_eq!(
                unset(&editor.steps[1].else_children[0]),
                vec![("own".to_string(), true)],
                "“Else” runs with nothing captured"
            );
            assert_eq!(unset(&editor.steps[2]), Vec::new());
            assert_eq!(unset(&editor.steps[3].children[0]), Vec::new());
            assert_eq!(
                unset(&editor.steps[4].children[0]),
                vec![("once".to_string(), true)]
            );
        });

        // A macro that runs its steps more than once gives the first step
        // what the last one set on the pass before.
        editor.update_in(cx, |editor, window, cx| {
            editor.repeat.update(cx, |repeat, cx| {
                repeat.set_text("2", window, cx);
            });
        });
        editor.read_with(cx, |editor, cx| {
            let reach = editor.variable_reach(&editor.defined_variables(cx), cx);
            assert_eq!(editor.steps[0].undefined_references(&reach, cx), Vec::new());
        });

        assert_eq!(
            unset_reference_warnings(&[
                Unset {
                    name: "typo".to_string(),
                    set_later: false,
                },
                Unset {
                    name: "late".to_string(),
                    set_later: true,
                },
            ]),
            vec![
                "No step sets ${typo}. When the macro runs, it stays as written.".to_string(),
                "${late} gets its value only after this step. Here it stays as written."
                    .to_string(),
            ]
        );
    }

    #[gpui::test]
    async fn a_regex_row_is_compiled_once_per_text_and_warns_about_a_repeated_reference(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            let terminal_macro = TerminalMacro {
                name: "正規表現".to_string(),
                steps: vec![MacroStep::If {
                    pattern: r"^${p}+ (?<ver>\S+)".to_string(),
                    regex: true,
                    scope: MatchScope::Screen,
                    capture: None,
                    then_steps: Vec::new(),
                    else_steps: Vec::new(),
                }],
                ..TerminalMacro::default()
            };
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        for _ in 0..3 {
            editor.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        }

        editor.read_with(cx, |editor, cx| {
            let row = &editor.steps[0];
            assert_eq!(
                row.pattern_compilations.get(),
                1,
                "drawing the form again must not compile the pattern again"
            );
            assert_eq!(row.own_variables(cx), vec!["ver".to_string()]);
            assert_eq!(row.pattern_compilations.get(), 1);
            assert_eq!(row.repeated_references(cx), vec!["p".to_string()]);
            assert_eq!(row.pattern_error(cx), None);
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("(?<ver>", window, cx);
            });
        });
        cx.run_until_parked();
        editor.read_with(cx, |editor, cx| {
            let row = &editor.steps[0];
            assert_eq!(
                row.pattern_compilations.get(),
                2,
                "a new text is compiled once"
            );
            assert_eq!(row.own_variables(cx), Vec::<String>::new());
            let error = row
                .pattern_error(cx)
                .expect("an unclosed group does not compile");
            assert!(!error.contains('\n'), "one line under the field: {error}");
            assert_eq!(row.pattern_compilations.get(), 2);
        });
    }

    #[gpui::test]
    async fn a_step_this_build_cannot_read_is_saved_back_untouched(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "future", "steps": [
                { "type": "send", "text": "show version" },
                { "type": "beep", "pitch": 440 }
            ] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let terminal_macro = stored_macro(&editor, None, "future", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        cx.run_until_parked();
        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.steps[1]
                    .unknown
                    .as_ref()
                    .and_then(|step| step.unknown_kind()),
                Some("beep")
            );
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("show clock", window, cx);
            });
            editor.save_form(cx).detach();
            assert_eq!(editor.error, None);
        });
        cx.run_until_parked();

        let saved = saved_file(&fs)
            .await
            .macro_named(None, "future")
            .map(|terminal_macro| terminal_macro.steps.clone());
        assert_eq!(
            saved,
            Some(vec![
                MacroStep::Send {
                    text: "show clock".to_string(),
                    newline: true,
                },
                MacroStep::Unknown(serde_json::json!({ "type": "beep", "pitch": 440 })),
            ])
        );
    }

    #[gpui::test]
    async fn a_capture_name_written_as_a_reference_is_read_as_the_name(cx: &mut TestAppContext) {
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
                capture.set_text(" ${host} ", window, cx);
            });
            editor.save_form(cx).detach();
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
                timeout_ms: terminal::terminal_macro::DEFAULT_WAIT_TIMEOUT_MS,
                capture: Some("host".to_string()),
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

        // A wide form puts the two sides next to each other…
        let then_side = cx
            .debug_bounds("macro-step-2-1")
            .expect("the “Then” row should have been drawn");
        let else_side = cx
            .debug_bounds("macro-step-2-e1")
            .expect("the “Else” row should have been drawn");
        assert!(
            then_side.right() <= else_side.left() && then_side.top() == else_side.top(),
            "“Then” spans {:?} and “Else” {:?}, which are not side by side",
            then_side,
            else_side
        );

        // …and one too narrow for two lanes stacks them.
        // (The tab hides the macro list at this width, which gives the form
        // the whole of it.)
        cx.simulate_resize(size(px(420.), px(900.)));
        settle(cx);
        let then_side = cx
            .debug_bounds("macro-step-2-1")
            .expect("the “Then” row should have been drawn");
        let else_side = cx
            .debug_bounds("macro-step-2-e1")
            .expect("the “Else” row should have been drawn");
        assert!(
            then_side.bottom() <= else_side.top(),
            "“Then” ends at {} but “Else” starts at {}",
            then_side.bottom(),
            else_side.top()
        );
    }

    #[gpui::test]
    async fn a_narrow_form_keeps_step_titles_off_their_buttons(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.set_macro_list_shown(false, cx);
            editor.load_macro(Some(branching_macro()), window, cx);
        });
        cx.simulate_resize(size(px(900.), px(900.)));
        settle(cx);

        let steps = ["macro-step-1", "macro-step-2", "macro-step-2-1"];
        let bounds = |cx: &mut VisualTestContext, name: String| {
            cx.debug_bounds(Box::leak(name.clone().into_boxed_str()))
                .unwrap_or_else(|| panic!("{name} should have been drawn"))
        };
        let natural_kind_widths: Vec<Pixels> = steps
            .iter()
            .map(|step| bounds(cx, format!("{step}-kind")).size.width)
            .collect();

        let mut truncated = false;
        for width in [px(420.), px(300.), px(240.), px(200.)] {
            cx.simulate_resize(size(width, px(900.)));
            settle(cx);
            let form = bounds(cx, "macro-form".to_string());
            let interval = bounds(cx, "macro-repeat-interval".to_string());
            assert!(
                interval.right() <= form.right(),
                "at {width:?} the interval field ends at {} past the form at {}",
                interval.right(),
                form.right()
            );
            for (step, natural) in steps.iter().zip(&natural_kind_widths) {
                let header = bounds(cx, format!("{step}-header"));
                let title = bounds(cx, format!("{step}-title"));
                let kind = bounds(cx, format!("{step}-kind"));
                let buttons = bounds(cx, format!("{step}-header-buttons"));
                assert!(
                    title.right() <= buttons.left(),
                    "at {width:?} the title of {step} ends at {} under its buttons from {}",
                    title.right(),
                    buttons.left()
                );
                assert!(
                    buttons.right() <= header.right() + px(0.5),
                    "at {width:?} the buttons of {step} end at {} past the card at {}",
                    buttons.right(),
                    header.right()
                );
                if kind.size.width > px(0.) {
                    assert!(
                        kind.right() <= title.right() + px(0.5),
                        "at {width:?} the kind of {step} ends at {} past its title at {}",
                        kind.right(),
                        title.right()
                    );
                }
                if kind.size.width + px(1.) < *natural {
                    truncated = true;
                }
            }
        }
        assert!(truncated, "no width was narrow enough to shorten a title");

        // Narrower than the buttons alone: drawn without panicking, and the
        // layout comes to rest instead of asking for frame after frame.
        for width in [px(100.), px(40.), px(1.)] {
            cx.simulate_resize(size(width, px(900.)));
            settle(cx);
            let pending = cx.update(|window, cx| window.simulate_next_frame(cx));
            assert_eq!(pending, 0, "at {width:?} the form keeps redrawing");
        }
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
                "{selector} is past depth {MAX_EDITABLE_STEP_DEPTH} and is only editable as JSON"
            );
        }

        editor.update_in(cx, |editor, _, cx| editor.save_form(cx).detach());
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
                        on_timeout: Some(vec![MacroStep::Clear {}, MacroStep::Sleep { ms: 500 }]),
                    },
                    MacroStep::Clear {},
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
                .expect("the “Else” list is the row's own list");
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
                .expect("the “On Timeout” list is the row's own list");
            if deleted.index() < siblings.len() {
                siblings.remove(deleted.index());
            }

            editor.save_form(cx).detach();
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
                    MacroStep::Clear {},
                    MacroStep::Sleep { ms: 2_000 },
                ],
                else_steps: vec![send("一"), send("三"), send("二")],
            }]
        );
    }

    fn sent(text: &str) -> MacroStep {
        MacroStep::Send {
            text: text.to_string(),
            newline: true,
        }
    }

    /// The form's steps as one line, a 送信 standing for itself by its text:
    /// `a b 分岐(t1 t2 | e1) 繰り返し(r1) c`.
    fn outline(rows: &[StepRow], cx: &App) -> String {
        rows.iter()
            .map(|row| match row.kind {
                StepKind::Send => row.text.read(cx).text(cx),
                StepKind::If => format!(
                    "分岐({} | {})",
                    outline(&row.children, cx),
                    outline(&row.else_children, cx)
                ),
                StepKind::Repeat => format!("繰り返し({})", outline(&row.children, cx)),
                kind => kind.label().to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn reorder_macro() -> TerminalMacro {
        TerminalMacro {
            name: "並べ替え".to_string(),
            steps: vec![
                sent("a"),
                sent("b"),
                MacroStep::If {
                    pattern: "点検".to_string(),
                    regex: false,
                    scope: MatchScope::SinceLastSend,
                    capture: None,
                    then_steps: vec![sent("t1"), sent("t2")],
                    else_steps: vec![sent("e1")],
                },
                MacroStep::Repeat {
                    count: 2,
                    steps: vec![sent("r1")],
                },
                sent("c"),
            ],
            ..TerminalMacro::default()
        }
    }

    fn at(parent: Option<(StepPath, StepList)>, index: usize) -> InsertionPoint {
        InsertionPoint { parent, index }
    }

    #[gpui::test]
    async fn moving_a_row_lands_it_where_the_drop_was_drawn(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let editor = open_editor(&workspace, &store, cx);

        let branch = StepPath::top(2);
        let repeat = StepPath::top(3);
        let original = "a b 分岐(t1 t2 | e1) 繰り返し(r1) c";
        // (what, row moved, where it is dropped, the form after it, or `None`
        // for a drop that has to be refused and change nothing).
        let cases: Vec<(&str, StepPath, InsertionPoint, Option<&str>)> = vec![
            (
                "later in the same list: the drop counts positions before the row left",
                StepPath::top(0),
                at(None, 2),
                Some("b a 分岐(t1 t2 | e1) 繰り返し(r1) c"),
            ),
            (
                "earlier in the same list",
                StepPath::top(4),
                at(None, 0),
                Some("c a b 分岐(t1 t2 | e1) 繰り返し(r1)"),
            ),
            (
                "the “+” at the end of its own list",
                StepPath::top(0),
                at(None, 5),
                Some("b 分岐(t1 t2 | e1) 繰り返し(r1) c a"),
            ),
            (
                "from 「そのとき」 to 「それ以外」",
                branch.child(StepList::Main, 1),
                at(Some((branch.clone(), StepList::Else)), 0),
                Some("a b 分岐(t1 | t2 e1) 繰り返し(r1) c"),
            ),
            (
                "into a list whose card comes after the row, which moves up one",
                StepPath::top(1),
                at(Some((branch.clone(), StepList::Main)), 0),
                Some("a 分岐(b t1 t2 | e1) 繰り返し(r1) c"),
            ),
            (
                "out of a repeat to the top of the macro",
                repeat.child(StepList::Main, 0),
                at(None, 0),
                Some("r1 a b 分岐(t1 t2 | e1) 繰り返し() c"),
            ),
            (
                "out of “Else” to just after its branch",
                branch.child(StepList::Else, 0),
                at(None, 3),
                Some("a b 分岐(t1 t2 | ) e1 繰り返し(r1) c"),
            ),
            (
                "from one card's list into another card's",
                branch.child(StepList::Main, 1),
                at(Some((repeat, StepList::Main)), 1),
                Some("a b 分岐(t1 | e1) 繰り返し(r1 t2) c"),
            ),
            (
                "into its own “Then”",
                branch.clone(),
                at(Some((branch.clone(), StepList::Main)), 0),
                None,
            ),
            (
                "into its own “Else”, at the end",
                branch.clone(),
                at(Some((branch, StepList::Else)), 1),
                None,
            ),
            (
                "onto the “+” right before itself",
                StepPath::top(1),
                at(None, 1),
                None,
            ),
            (
                "onto the “+” right after itself",
                StepPath::top(1),
                at(None, 2),
                None,
            ),
            (
                "past the end of the list",
                StepPath::top(0),
                at(None, 6),
                None,
            ),
            (
                "into a card that does not exist",
                StepPath::top(0),
                at(Some((StepPath::top(9), StepList::Main)), 0),
                None,
            ),
            (
                "a row that does not exist",
                StepPath::top(9),
                at(None, 0),
                None,
            ),
        ];

        for (what, from, to, expected) in cases {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(reorder_macro()), window, cx);
                let moved = editor.move_row(&from, to.clone());
                assert_eq!(moved, expected.is_some(), "{what}");
                assert_eq!(
                    outline(&editor.steps, cx),
                    expected.unwrap_or(original),
                    "{what}"
                );
            });
        }

        // Deeper than one level: the outer 繰り返し cannot go into the 分岐
        // inside it either.
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(deeply_nested_macro()), window, cx);
            let inner_branch = StepPath::top(0).child(StepList::Main, 0);
            assert!(!editor.move_row(
                &StepPath::top(0),
                at(Some((inner_branch.clone(), StepList::Else)), 0),
            ));
            assert!(
                !InsertionPoint {
                    parent: Some((inner_branch, StepList::Main)),
                    index: 0,
                }
                .accepts(&StepPath::top(0))
            );
        });

        // The row's fields travel with it: the moved row still holds what was
        // typed into it, in the very editors it had.
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(reorder_macro()), window, cx);
            editor.steps[0].text.update(cx, |text, cx| {
                text.set_text("a (typed)", window, cx);
            });
            let editors = editor.steps[0].text.entity_id();
            assert!(editor.move_row(
                &StepPath::top(0),
                at(Some((StepPath::top(2), StepList::Else)), 1),
            ));
            let moved = &editor.steps[1].else_children[1];
            assert_eq!(moved.text.entity_id(), editors);
            assert_eq!(moved.text.read(cx).text(cx), "a (typed)");
        });
    }

    /// Presses on `from`, moves past the drag threshold, and lets go on `to`.
    fn drag(cx: &mut VisualTestContext, from: &'static str, to: &'static str) {
        let from = cx
            .debug_bounds(from)
            .unwrap_or_else(|| panic!("{from} should be on screen"));
        let to = cx
            .debug_bounds(to)
            .unwrap_or_else(|| panic!("{to} should be on screen"));
        cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            from.center() + point(px(20.), px(0.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_move(to.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn dragging_a_card_by_its_header_onto_a_plus_moves_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "ドラッグ".to_string(),
                    steps: vec![
                        sent("a"),
                        sent("b"),
                        sent("c"),
                        MacroStep::If {
                            pattern: "点検".to_string(),
                            regex: false,
                            scope: MatchScope::SinceLastSend,
                            capture: None,
                            then_steps: vec![sent("t1")],
                            else_steps: Vec::new(),
                        },
                    ],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
        });
        // Folded, every card and 「+」 fits in the form without scrolling.
        editor.update(cx, |editor, cx| editor.set_all_expanded(false, cx));
        cx.run_until_parked();

        drag(cx, "macro-step-1-header", "macro-insert-top-4");
        editor.read_with(cx, |editor, cx| {
            assert_eq!(outline(&editor.steps, cx), "b c 分岐(t1 | ) a");
        });

        // Across lanes: into the empty 「それ以外」 of the 分岐, now step 3.
        drag(cx, "macro-step-4-header", "macro-insert-3-else-0");
        editor.read_with(cx, |editor, cx| {
            assert_eq!(outline(&editor.steps, cx), "b c 分岐(t1 | a)");
        });

        // A card dropped into itself stays where it is.
        drag(cx, "macro-step-3-header", "macro-insert-3-main-0");
        editor.read_with(cx, |editor, cx| {
            assert_eq!(outline(&editor.steps, cx), "b c 分岐(t1 | a)");
        });
    }

    #[gpui::test]
    async fn a_drop_after_the_rows_changed_under_the_drag_moves_nothing(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "途中で消える".to_string(),
                    steps: vec![sent("a"), sent("b"), sent("c"), sent("d")],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            editor.set_all_expanded(false, cx);
        });
        cx.run_until_parked();

        let from = cx
            .debug_bounds("macro-step-1-header")
            .expect("step 1 should be on screen");
        cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(
            from.center() + point(px(20.), px(0.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        // The dragged 「a」 goes away mid-drag, so the path the drag carries
        // now leads to 「b」.
        editor.update(cx, |editor, cx| {
            assert!(editor.delete_step(&StepPath::top(0)));
            cx.notify();
        });
        cx.run_until_parked();
        let to = cx
            .debug_bounds("macro-insert-top-3")
            .expect("the “+” at the end should be on screen");
        cx.simulate_mouse_move(to.center(), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to.center(), MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        editor.read_with(cx, |editor, cx| {
            assert_eq!(outline(&editor.steps, cx), "b c d");
        });
    }

    #[gpui::test]
    async fn moving_a_row_between_nested_lists_finds_the_list_it_was_dropped_in(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();
        let editor = open_editor(&workspace, &store, cx);

        let branch = |then_steps: Vec<MacroStep>, else_steps: Vec<MacroStep>| MacroStep::If {
            pattern: "点検".to_string(),
            regex: false,
            scope: MatchScope::SinceLastSend,
            capture: None,
            then_steps,
            else_steps,
        };
        let repeat = |steps: Vec<MacroStep>| MacroStep::Repeat { count: 2, steps };
        let nested = TerminalMacro {
            name: "入れ子".to_string(),
            steps: vec![
                branch(
                    vec![sent("t1"), repeat(vec![sent("r1")])],
                    vec![sent("e1"), repeat(vec![sent("q1")])],
                ),
                branch(vec![sent("u1"), repeat(vec![sent("w1")])], Vec::new()),
            ],
            ..TerminalMacro::default()
        };
        let first = StepPath::top(0);
        let second = StepPath::top(1);
        let cases: Vec<(&str, StepPath, InsertionPoint, &str)> = vec![
            (
                "into the repeat after it in the same “Then”, which moves up one",
                first.child(StepList::Main, 0),
                at(Some((first.child(StepList::Main, 1), StepList::Main)), 0),
                "分岐(繰り返し(t1 r1) | e1 繰り返し(q1)) 分岐(u1 繰り返し(w1) | )",
            ),
            (
                "from “Else” into the repeat of “Then”, which stays put",
                first.child(StepList::Else, 0),
                at(Some((first.child(StepList::Main, 1), StepList::Main)), 1),
                "分岐(t1 繰り返し(r1 e1) | 繰り返し(q1)) 分岐(u1 繰り返し(w1) | )",
            ),
            (
                "into the second branch's repeat, whose path only looks like a later sibling's",
                first.child(StepList::Main, 0),
                at(Some((second.child(StepList::Main, 1), StepList::Main)), 0),
                "分岐(繰り返し(r1) | e1 繰り返し(q1)) 分岐(u1 繰り返し(t1 w1) | )",
            ),
            (
                "into the next branch from inside the first, which does not shift it",
                first.child(StepList::Main, 0),
                at(Some((second.clone(), StepList::Main)), 0),
                "分岐(繰り返し(r1) | e1 繰り返し(q1)) 分岐(t1 u1 繰り返し(w1) | )",
            ),
            (
                "two levels up, to the other side of its branch",
                first.child(StepList::Main, 1).child(StepList::Main, 0),
                at(Some((first.clone(), StepList::Else)), 0),
                "分岐(t1 繰り返し() | r1 e1 繰り返し(q1)) 分岐(u1 繰り返し(w1) | )",
            ),
            (
                "a whole branch into a list two levels inside the card after it",
                first,
                at(Some((second.child(StepList::Main, 1), StepList::Main)), 1),
                "分岐(u1 繰り返し(w1 分岐(t1 繰り返し(r1) | e1 繰り返し(q1))) | )",
            ),
        ];
        for (what, from, to, expected) in cases {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(nested.clone()), window, cx);
                assert!(editor.move_row(&from, to.clone()), "{what}");
                assert_eq!(outline(&editor.steps, cx), expected, "{what}");
            });
        }
    }

    #[gpui::test]
    async fn a_step_put_in_between_renumbers_the_steps_after_it(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "挿入".to_string(),
                    steps: vec![
                        sent("a"),
                        sent("b"),
                        MacroStep::If {
                            pattern: "点検".to_string(),
                            regex: false,
                            scope: MatchScope::SinceLastSend,
                            capture: None,
                            then_steps: vec![sent("t1")],
                            else_steps: Vec::new(),
                        },
                    ],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("macro-step-4").is_none());

        editor.update_in(cx, |editor, window, cx| {
            editor.insert_step(at(None, 1), StepKind::Sleep, window, cx);
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            assert_eq!(outline(&editor.steps, cx), "a Sleep b 分岐(t1 | )");
            let inserted = &editor.steps[1];
            assert!(inserted.expanded, "a new card opens unfolded");
            assert!(
                inserted
                    .number
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                "the new sleep should be ready to type its seconds into"
            );
        });
        let first = cx
            .debug_bounds("macro-step-1")
            .expect("step 1 should have been drawn");
        let inserted = cx
            .debug_bounds("macro-step-2")
            .expect("the inserted step should be numbered 2");
        let pushed_down = cx
            .debug_bounds("macro-step-3")
            .expect("the old step 2 should now be step 3");
        assert!(
            first.bottom() <= inserted.top() && inserted.bottom() <= pushed_down.top(),
            "the inserted card should be drawn between steps 1 and 3"
        );
        assert!(
            cx.debug_bounds("macro-step-4-1").is_some(),
            "the branch is step 4 now, and its “Then” is numbered after it"
        );

        // Into the empty 「それ以外」 lane, through its 「+」.
        assert!(cx.debug_bounds("macro-insert-4-else-0").is_some());
        editor.update_in(cx, |editor, window, cx| {
            editor.insert_step(
                at(Some((StepPath::top(3), StepList::Else)), 0),
                StepKind::SetVariable,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        editor.update_in(cx, |editor, window, cx| {
            assert_eq!(
                outline(&editor.steps, cx),
                "a Sleep b 分岐(t1 | Set Variable)"
            );
            assert!(
                editor.steps[3].else_children[0]
                    .capture
                    .focus_handle(cx)
                    .contains_focused(window, cx),
                "the new set-variable step should be ready to type its variable name into"
            );
        });
        assert!(cx.debug_bounds("macro-step-4-e1").is_some());
    }

    fn repeat_of(count: usize) -> TerminalMacro {
        TerminalMacro {
            name: format!("{count} 枚"),
            steps: vec![MacroStep::Repeat {
                count: 2,
                steps: (0..count - 1)
                    .map(|index| sent(&index.to_string()))
                    .collect(),
            }],
            ..TerminalMacro::default()
        }
    }

    fn every_card_expanded(rows: &[StepRow]) -> Vec<bool> {
        rows.iter()
            .flat_map(|row| std::iter::once(row.expanded).chain(every_card_expanded(&row.children)))
            .collect()
    }

    #[gpui::test]
    async fn a_macro_opens_with_every_card_folded(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            // However few cards there are, the steps inside the 繰り返し too.
            editor.load_macro(Some(repeat_of(2)), window, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false; 2]);

            editor.load_macro(Some(repeat_of(9)), window, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false; 9]);

            editor.set_all_expanded(true, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![true; 9]);
            editor.set_all_expanded(false, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false; 9]);

            let one_step = |text: &str| TerminalMacro {
                name: "1 枚".to_string(),
                steps: vec![sent(text)],
                ..TerminalMacro::default()
            };
            editor.load_macro(Some(one_step("")), window, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false]);
            editor.load_macro(Some(one_step("show version")), window, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false]);

            // A card put in after opening is there to be typed into.
            editor.add_step(StepKind::Sleep, window, cx);
            assert_eq!(every_card_expanded(&editor.steps), vec![false, true]);
        });
    }

    #[gpui::test]
    async fn a_branch_with_one_empty_side_gives_its_width_to_the_other(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let branch = |then_steps: Vec<MacroStep>, else_steps: Vec<MacroStep>| TerminalMacro {
            name: "分岐".to_string(),
            steps: vec![MacroStep::If {
                pattern: "router#".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: None,
                then_steps,
                else_steps,
            }],
            ..TerminalMacro::default()
        };
        let narrow = cx.update(|window, _| rems(NARROW_LANE_WIDTH).to_pixels(window.rem_size()));
        let editor = open_editor(&workspace, &store, cx);
        for (then_steps, else_steps, empty_lane, full_lane, empty_insert) in [
            (
                vec![sent("a")],
                Vec::new(),
                "macro-lane-1-else",
                "macro-lane-1-main",
                "macro-insert-1-else-0",
            ),
            (
                Vec::new(),
                vec![sent("b")],
                "macro-lane-1-main",
                "macro-lane-1-else",
                "macro-insert-1-main-0",
            ),
        ] {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(branch(then_steps, else_steps)), window, cx);
            });
            cx.run_until_parked();
            let empty = cx
                .debug_bounds(empty_lane)
                .expect("the empty side should have been drawn");
            let full = cx
                .debug_bounds(full_lane)
                .expect("the side with a step should have been drawn");
            assert!(
                (empty.top() - full.top()).abs() < px(1.),
                "the sides should be beside each other: {empty:?} {full:?}"
            );
            assert!(
                (empty.size.width - narrow).abs() <= px(1.),
                "the empty side is {:?} wide, not the narrow {narrow:?}",
                empty.size.width
            );
            assert!(
                full.size.width > empty.size.width * 2.,
                "the side with a step should take the rest: {full:?} {empty:?}"
            );
            let insert = cx
                .debug_bounds(empty_insert)
                .expect("the empty side keeps its “+”");
            assert!(
                insert.size.width > px(0.)
                    && insert.left() >= empty.left()
                    && insert.right() <= empty.right()
                    && insert.top() >= empty.top()
                    && insert.bottom() <= empty.bottom(),
                "the “+” {insert:?} should be inside the empty side {empty:?}"
            );
        }

        for (then_steps, else_steps) in
            [(vec![sent("a")], vec![sent("b")]), (Vec::new(), Vec::new())]
        {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(branch(then_steps, else_steps)), window, cx);
            });
            cx.run_until_parked();
            let then_lane = cx
                .debug_bounds("macro-lane-1-main")
                .expect("“Then” should have been drawn");
            let else_lane = cx
                .debug_bounds("macro-lane-1-else")
                .expect("“Else” should have been drawn");
            assert!((then_lane.top() - else_lane.top()).abs() < px(1.));
            assert!(
                (then_lane.size.width - else_lane.size.width).abs() <= px(1.),
                "sides that are both empty or both not share the width: {then_lane:?} {else_lane:?}"
            );
            assert!(then_lane.size.width > narrow * 2.);
        }

        // A 分岐 inside the side that was given the width is laid out by that
        // width: here it only fits side by side in the wide side, not in half.
        let width = cx.update(|window, _| rems(55.).to_pixels(window.rem_size()));
        cx.simulate_resize(size(width, px(900.)));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_macro_list_shown(false, cx);
            let inner = MacroStep::If {
                pattern: "#".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: None,
                then_steps: vec![sent("c")],
                else_steps: vec![sent("d")],
            };
            editor.load_macro(Some(branch(vec![inner], Vec::new())), window, cx);
        });
        settle(cx);
        let outer = cx
            .debug_bounds("macro-lane-1-main")
            .expect("the outer “Then” should have been drawn");
        let then_lane = cx
            .debug_bounds("macro-lane-1-1-main")
            .expect("the inner “Then” should have been drawn");
        let else_lane = cx
            .debug_bounds("macro-lane-1-1-else")
            .expect("the inner “Else” should have been drawn");
        let min_lane = cx.update(|window, _| rems(MIN_LANE_WIDTH).to_pixels(window.rem_size()));
        assert!(
            outer.size.width / 2. < min_lane * 2.,
            "the window should be too narrow for the inner lanes in half the outer {outer:?}"
        );
        assert!(
            (then_lane.top() - else_lane.top()).abs() < px(1.)
                && then_lane.right() <= else_lane.left(),
            "the inner lanes {then_lane:?} {else_lane:?} should be side by side"
        );
    }

    #[gpui::test]
    async fn a_branch_with_one_empty_side_stays_side_by_side_where_two_full_sides_stack(
        cx: &mut TestAppContext,
    ) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let branch = |then_steps: Vec<MacroStep>, else_steps: Vec<MacroStep>| TerminalMacro {
            name: "分岐".to_string(),
            steps: vec![MacroStep::If {
                pattern: "router#".to_string(),
                regex: false,
                scope: MatchScope::Screen,
                capture: None,
                then_steps,
                else_steps,
            }],
            ..TerminalMacro::default()
        };
        let width = cx.update(|window, _| rems(30.).to_pixels(window.rem_size()));
        cx.simulate_resize(size(width, px(900.)));
        let editor = open_editor(&workspace, &store, cx);
        editor.update(cx, |editor, cx| editor.set_macro_list_shown(false, cx));
        let lanes = |then_steps, else_steps, cx: &mut VisualTestContext| {
            editor.update_in(cx, |editor, window, cx| {
                editor.load_macro(Some(branch(then_steps, else_steps)), window, cx);
            });
            settle(cx);
            let then_lane = cx
                .debug_bounds("macro-lane-1-main")
                .expect("“Then” should have been drawn");
            let else_lane = cx
                .debug_bounds("macro-lane-1-else")
                .expect("“Else” should have been drawn");
            (then_lane, else_lane)
        };

        let (then_lane, else_lane) = lanes(vec![sent("a")], vec![sent("b")], cx);
        assert!(
            else_lane.top() >= then_lane.bottom(),
            "two sides with cards do not both fit their minimum here and stack: \
             {then_lane:?} {else_lane:?}"
        );

        let (then_lane, else_lane) = lanes(vec![sent("a")], Vec::new(), cx);
        assert!(
            (then_lane.top() - else_lane.top()).abs() < px(1.)
                && then_lane.right() <= else_lane.left(),
            "an empty side only needs its narrow width, so the sides stay beside \
             each other: {then_lane:?} {else_lane:?}"
        );
        let min_lane = cx.update(|window, _| rems(MIN_LANE_WIDTH).to_pixels(window.rem_size()));
        assert!(
            then_lane.size.width >= min_lane,
            "the side with cards still gets a lane's minimum: {then_lane:?}"
        );
    }

    #[gpui::test]
    async fn the_sides_of_a_branch_past_the_drawn_depth_are_stacked_lines(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let mut edited = deeply_nested_macro();
        fn deepest_branch(steps: &mut [MacroStep]) -> Option<&mut Vec<MacroStep>> {
            for step in steps {
                let found = match step {
                    MacroStep::Repeat { steps, .. } => deepest_branch(steps),
                    MacroStep::WaitFor {
                        on_timeout: Some(steps),
                        ..
                    } => deepest_branch(steps),
                    MacroStep::If {
                        pattern,
                        then_steps,
                        else_steps,
                        ..
                    } => {
                        if pattern == "五段目" {
                            return Some(else_steps);
                        }
                        deepest_branch(then_steps)
                    }
                    _ => None,
                };
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        deepest_branch(&mut edited.steps)
            .expect("the nested macro has a branch at the deepest drawn level")
            .clear();

        let width = cx.update(|window, _| rems(120.).to_pixels(window.rem_size()));
        cx.simulate_resize(size(width, px(900.)));
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.set_macro_list_shown(false, cx);
            editor.load_macro(Some(edited), window, cx);
        });
        settle(cx);

        let then_lane = cx
            .debug_bounds("macro-lane-1-1-1-1-t1-main")
            .expect("“Then” past the drawn depth is a line of text");
        let else_lane = cx
            .debug_bounds("macro-lane-1-1-1-1-t1-else")
            .expect("“Else” past the drawn depth is a line of text");
        assert!(
            else_lane.top() >= then_lane.bottom(),
            "the two lines are stacked: {then_lane:?} {else_lane:?}"
        );
        assert!(
            (then_lane.size.width - else_lane.size.width).abs() <= px(1.),
            "the empty side is not squeezed to the narrow width, where its line \
             would wrap: {then_lane:?} {else_lane:?}"
        );
    }

    #[test]
    fn the_space_a_card_and_a_lane_take_follows_the_font_size() {
        assert_eq!(card_chrome(px(16.)), px(2. + 3. + 12.));
        assert_eq!(card_chrome(px(20.)), px(2. + 3. + 15.));
        assert_eq!(lane_chrome(px(16.)), px(10.));
        assert_eq!(lane_chrome(px(20.)), px(12.));

        let rem = px(16.);
        let gap = px(8.);
        let one_empty = rems(MIN_LANE_WIDTH + NARROW_LANE_WIDTH).to_pixels(rem) + gap;
        let both_full = rems(MIN_LANE_WIDTH * 2.).to_pixels(rem) + gap;
        assert!(branch_sides_fit(one_empty, true, gap, rem));
        assert!(!branch_sides_fit(one_empty - px(1.), true, gap, rem));
        assert!(!branch_sides_fit(one_empty, false, gap, rem));
        assert!(branch_sides_fit(both_full, false, gap, rem));
        assert!(!branch_sides_fit(both_full - px(1.), false, gap, rem));
    }

    #[gpui::test]
    async fn a_value_set_only_where_a_timeout_stops_the_macro_is_set_nowhere(
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
                    name: "タイムアウト".to_string(),
                    steps: vec![
                        MacroStep::WaitFor {
                            pattern: "router#".to_string(),
                            regex: false,
                            timeout_ms: 1_000,
                            capture: None,
                            on_timeout: Some(vec![MacroStep::SetVariable {
                                name: "x".to_string(),
                                value: "1".to_string(),
                            }]),
                        },
                        sent("echo ${x}"),
                        MacroStep::If {
                            pattern: "^${p}{,3}".to_string(),
                            regex: true,
                            scope: MatchScope::Screen,
                            capture: None,
                            then_steps: Vec::new(),
                            else_steps: Vec::new(),
                        },
                    ],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
        });
        let notes = |editor: &MacroEditor, index: usize, cx: &App| {
            let defined = editor.defined_variables(cx);
            let reach = editor.variable_reach(&defined, cx);
            editor
                .step_notes(&editor.steps[index], &reach, cx)
                .into_iter()
                .map(|(note, _)| note)
                .collect::<Vec<_>>()
        };
        editor.read_with(cx, |editor, cx| {
            assert_eq!(
                notes(editor, 1, cx),
                Vec::<String>::new(),
                "a wait that carries on may well have set it"
            );
        });

        // 「マクロを中止する」: the timeout list is not part of the macro, so
        // nothing sets the value, before or after.
        editor.update(cx, |editor, _| {
            if let Some(row) = editor.row_mut(&StepPath::top(0)) {
                row.continue_on_timeout = false;
            }
        });
        editor.read_with(cx, |editor, cx| {
            assert!(
                !editor
                    .defined_variables(cx)
                    .iter()
                    .any(|(name, _)| name == "x"),
                "the variable list leaves it out"
            );
            assert_eq!(
                notes(editor, 1, cx),
                vec!["No step sets ${x}. When the macro runs, it stays as written.".to_string()]
            );
            // `{,3}` is no repetition to the regex crate: the pattern is
            // refused as it stands.
            assert!(
                editor.steps[2].pattern_error(cx).is_some(),
                "“^${{p}}{{,3}}” should be reported as an invalid regular expression"
            );
        });
    }

    #[gpui::test]
    async fn a_folded_card_shows_that_something_in_it_is_wrong(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "壊れた正規表現".to_string(),
                    steps: vec![
                        MacroStep::If {
                            pattern: "(?<ver>".to_string(),
                            regex: true,
                            scope: MatchScope::Screen,
                            capture: None,
                            then_steps: Vec::new(),
                            else_steps: Vec::new(),
                        },
                        sent("show version"),
                    ],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            editor.set_all_expanded(true, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("macro-step-1-notes").is_none(),
            "an unfolded card lists the error under its fields instead"
        );

        editor.update(cx, |editor, cx| editor.set_all_expanded(false, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("macro-step-1-notes").is_some(),
            "a folded card with a broken regex has to say so in its header"
        );
        assert!(
            cx.debug_bounds("macro-step-2-notes").is_none(),
            "a card with nothing wrong gets no mark"
        );
        editor.read_with(cx, |editor, cx| {
            let defined = editor.defined_variables(cx);
            let reach = editor.variable_reach(&defined, cx);
            let notes = editor.step_notes(&editor.steps[0], &reach, cx);
            assert!(
                matches!(notes.first(), Some((note, Color::Error)) if note.starts_with("Invalid regular expression")),
                "{notes:?}"
            );
        });
    }

    #[gpui::test]
    async fn folding_cards_is_not_an_edit_and_does_not_change_what_is_saved(
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
            editor.set_all_expanded(false, cx);
            if let Some(row) = editor.row_mut(&StepPath::top(1).child(StepList::Main, 0)) {
                row.expanded = true;
            }
            assert!(
                !editor.has_unsaved_edits(cx),
                "folding a card is how the form is looked at, not a change to the macro"
            );
            editor.save_form(cx).detach();
        });
        cx.run_until_parked();

        let contents = fs
            .load(Path::new(path!("/config/terminal_sessions.json")))
            .await
            .expect("the sessions file should have been written");
        assert_eq!(parse_sessions_file(&contents).macros, vec![edited]);
    }

    #[gpui::test]
    async fn the_macro_list_follows_the_width_of_the_tab(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ルーター", "steps": [{ "type": "clear" }] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        settle(cx);
        let rem_width = |width: f32, cx: &mut VisualTestContext| {
            cx.update(|window, _| rems(width).to_pixels(window.rem_size()))
        };

        let wide = rem_width(80., cx);
        let very_wide = rem_width(130., cx);
        let medium = rem_width(60., cx);
        cx.simulate_resize(size(wide, px(700.)));
        let editor = open_editor(&workspace, &store, cx);
        let terminal_macro = stored_macro(&editor, None, "ルーター", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(Some(terminal_macro), window, cx);
        });
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_some(),
            "a wide tab shows the list beside the form"
        );
        let tab = cx
            .debug_bounds("macro-editor")
            .expect("the editor should have been drawn");
        assert!(
            tab.left() >= px(0.) && tab.right() <= wide && tab.bottom() <= px(700.),
            "the tab {tab:?} runs off the window"
        );
        assert!(
            tab.size.width > wide - px(48.),
            "the tab {tab:?} should fill its pane, which is the whole width"
        );

        // The window stays as wide, but the tab gets only half of it: what
        // counts is the tab's own width.
        workspace.update_in(cx, |workspace, window, cx| {
            let other = cx.new(|cx| workspace::item::test::TestItem::new(cx));
            workspace.split_item(
                workspace::SplitDirection::Right,
                Box::new(other),
                window,
                cx,
            );
        });
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_none(),
            "half of the window is too narrow for the list"
        );
        assert!(
            cx.debug_bounds("macro-form-header").is_some(),
            "with the list hidden, the form's heading is what names the open macro"
        );
        editor.read_with(cx, |editor, _| {
            assert!(!editor.show_macro_list);
            assert_eq!(editor.form_heading(), "Edit “ルーター”");
        });

        // Wider again, the list comes back by itself; narrower, it goes.
        cx.simulate_resize(size(very_wide, px(700.)));
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_some(),
            "a tab widened past the threshold shows the list again"
        );
        cx.simulate_resize(size(wide, px(700.)));
        settle(cx);
        assert!(cx.debug_bounds("macro-list-pane").is_none());

        // Once the user has picked with the button, resizing leaves it be.
        let toggle = cx
            .debug_bounds("macro-list-toggle")
            .expect("the list button should have been drawn");
        cx.simulate_click(toggle.center(), Modifiers::none());
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_some(),
            "the button brings the list back"
        );
        cx.simulate_resize(size(very_wide, px(700.)));
        settle(cx);
        cx.simulate_resize(size(medium, px(700.)));
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_some(),
            "a list the user brought back stays through a resize"
        );
        // What the button does when the list is shown.
        editor.update(cx, |editor, cx| editor.set_macro_list_shown(false, cx));
        settle(cx);
        cx.simulate_resize(size(very_wide, px(700.)));
        settle(cx);
        assert!(
            cx.debug_bounds("macro-list-pane").is_none(),
            "a list the user hid stays hidden in a wide tab"
        );
    }

    #[gpui::test]
    async fn a_list_opened_in_a_narrow_tab_leaves_room_for_the_form(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{ "macros": [{ "name": "ルーター", "steps": [{ "type": "clear" }] }] }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        settle(cx);
        cx.simulate_resize(size(px(260.), px(700.)));
        let editor = open_editor(&workspace, &store, cx);
        settle(cx);
        assert!(cx.debug_bounds("macro-list-pane").is_none());

        editor.update(cx, |editor, cx| editor.set_macro_list_shown(true, cx));
        settle(cx);
        let tab = cx
            .debug_bounds("macro-editor")
            .expect("the editor should have been drawn");
        let list = cx
            .debug_bounds("macro-list-pane")
            .expect("the list the user opened should be drawn");
        let form = cx
            .debug_bounds("macro-form-frame")
            .expect("the form should be drawn");
        assert!(
            list.size.width <= tab.size.width / 2.,
            "the list {list:?} takes more than half of the tab {tab:?}"
        );
        assert!(
            form.size.width >= tab.size.width / 4. && form.right() <= tab.right(),
            "the form {form:?} has no room beside the list in the tab {tab:?}"
        );
    }

    #[gpui::test]
    async fn the_sides_of_a_branch_are_laid_out_by_the_width_of_the_tab(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        settle(cx);

        // Wide enough for the two sides as the only tab, not as half of the
        // window: a width taken from the window would put them side by side.
        let width = cx.update(|window, _| rems(64.).to_pixels(window.rem_size()));
        cx.simulate_resize(size(width, px(900.)));
        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.load_macro(
                Some(TerminalMacro {
                    name: "分岐".to_string(),
                    steps: vec![MacroStep::If {
                        pattern: "#".to_string(),
                        regex: false,
                        scope: MatchScope::Screen,
                        capture: None,
                        then_steps: vec![sent("a")],
                        else_steps: vec![sent("b")],
                    }],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            editor.set_macro_list_shown(false, cx);
        });
        settle(cx);
        let side_by_side = |cx: &mut VisualTestContext| {
            let then_lane = cx
                .debug_bounds("macro-lane-1-main")
                .expect("“Then” should have been drawn");
            let else_lane = cx
                .debug_bounds("macro-lane-1-else")
                .expect("“Else” should have been drawn");
            (then_lane.top() - else_lane.top()).abs() < px(1.)
        };
        assert!(side_by_side(cx), "the whole window fits both sides");

        workspace.update_in(cx, |workspace, window, cx| {
            let other = cx.new(|cx| workspace::item::test::TestItem::new(cx));
            workspace.split_item(
                workspace::SplitDirection::Right,
                Box::new(other),
                window,
                cx,
            );
        });
        settle(cx);
        let frame = cx
            .debug_bounds("macro-form-frame")
            .expect("the form should have been drawn");
        editor.read_with(cx, |editor, _| {
            assert_eq!(editor.measured_form_width.get(), Some(frame.size.width));
        });
        assert!(
            !side_by_side(cx),
            "in half of the window the sides of the branch stack"
        );
    }

    #[gpui::test]
    async fn a_branch_without_a_pattern_names_the_step(cx: &mut TestAppContext) {
        let (fs, workspace, window) = init_workspace(cx, r#"{ "macros": [] }"#).await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        settle(cx);

        let editor = open_editor(&workspace, &store, cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.name.update(cx, |name, cx| {
                name.set_text("分岐", window, cx);
            });
            editor.add_step(StepKind::Clear, window, cx);
            editor.add_step(StepKind::If, window, cx);
            editor.save_form(cx).detach();
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 2: enter the text to match.")
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
                capture.set_text("ho}st", window, cx);
            });
            editor.save_form(cx).detach();
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 1: the variable to capture into can't contain $, { or }.")
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
                capture.set_text("${ho$st}", window, cx);
            });
            editor.save_form(cx).detach();
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 1: the variable to capture into can't contain $, { or }.")
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
                                capture: Some("ho}st".to_string()),
                                on_timeout: None,
                            }]),
                        }],
                    }],
                    ..TerminalMacro::default()
                }),
                window,
                cx,
            );
            editor.save_form(cx).detach();
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 1-e1-t1: the variable to capture into can't contain $, { or }.")
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
            editor.save_form(cx).detach();
        });

        editor.read_with(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("Step 1: enter the repeat count as a whole number, 0 or more.")
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
            editor.save_form(cx).detach();
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
        assert_eq!(written.steps, vec![MacroStep::Clear {}]);
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
                        folder.unwrap_or_else(|| unfiled_folder().to_string())
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
        let store = editor.read_with(cx, |editor, _| editor.store.clone());
        stored_macro_in(&store, folder, name, cx)
    }

    fn stored_macro_in(
        store: &Entity<TerminalSessionStore>,
        folder: Option<&str>,
        name: &str,
        cx: &mut VisualTestContext,
    ) -> TerminalMacro {
        store.read_with(cx, |store, _| {
            store
                .macro_named(folder, name)
                .unwrap_or_else(|| panic!("“{name}” should be in the store"))
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
                "Unfiled/ (1)",
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
                "Unfiled/ (1)",
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
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "Unfiled/ (1)",]
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
            assert_eq!(editor.error.as_deref(), Some("Enter a folder name."));
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
                    { "name": "ログイン (copy)", "folder": "拠点/本社", "steps": [] },
                    { "name": "ログイン (copy 2)", "steps": [] }
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
            assert_eq!(editor.name.read(cx).text(cx), "ログイン (copy 2)");
            assert_eq!(editor.form_folder.as_deref(), Some("拠点/本社"));
            assert_eq!(editor.steps.len(), 1);
            assert!(
                editor.has_unsaved_edits(cx),
                "a copy is unsaved until Save, so switching away must warn"
            );
        });

        editor.update(cx, |editor, cx| editor.save_form(cx).detach());
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
                (Some("拠点/本社"), "ログイン (copy)"),
                (None, "ログイン (copy 2)"),
                (Some("拠点/本社"), "ログイン (copy 2)"),
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

    /// Copy names are saved as macro names, so the ones written by builds that
    /// only spoke Japanese are still in users' files and must keep counting as
    /// taken under the Japanese catalog.
    #[gpui::test]
    async fn duplicating_under_japanese_skips_copy_names_already_in_the_file(
        cx: &mut TestAppContext,
    ) {
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let (fs, workspace, window) = init_workspace(
            cx,
            r#"{
                "macros": [
                    { "name": "ログイン", "steps": [{ "type": "send", "text": "enable" }] },
                    { "name": "ログイン のコピー", "steps": [] },
                    { "name": "ログイン (copy)", "steps": [] }
                ]
            }"#,
        )
        .await;
        let cx = &mut VisualTestContext::from_window(*window, cx);
        let store = new_store(&fs, cx);
        cx.run_until_parked();

        let editor = open_editor(&workspace, &store, cx);
        let login = stored_macro(&editor, None, "ログイン", cx);
        editor.update_in(cx, |editor, window, cx| {
            editor.select_macro(Some(login), window, cx);
            editor.duplicate_macro(window, cx);
        });
        editor.update(cx, |editor, cx| {
            assert_eq!(editor.name.read(cx).text(cx), "ログイン のコピー 2");
        });
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
            vec!["ルータ用/ (0)", "ルータ用/ (空)", "Unfiled/ (1)", "メモ"]
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
                Some("A folder with this name already exists.")
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
            editor.save_form(cx).detach();
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
            editor.save_form(cx).detach();
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
            editor.save_form(cx).detach();
        });
        cx.run_until_parked();

        editor.update(cx, |editor, _| {
            assert_eq!(
                editor.error.as_deref(),
                Some("This folder already has a macro with this name.")
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
            editor.save_form(cx).detach();
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
            editor.save_form(cx).detach();
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
            editor.save_form(cx).detach();
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
                Some("A folder with this name already exists.")
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
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "Unfiled/ (1)"]
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
        assert_eq!(tree_rows(&editor, cx), vec!["Unfiled/ (1)", "メモ"]);

        // Clearing the filter puts both back the way the user folded them.
        filter(&editor, "", cx);
        assert_eq!(
            tree_rows(&editor, cx),
            vec!["拠点/大阪/ (1)", "再起動", "拠点/本社/ (2)", "Unfiled/ (1)"]
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

    fn no_proxy(_env: &mut HashMap<String, String>) -> bool {
        false
    }

    fn test_proxy(env: &mut HashMap<String, String>) -> bool {
        env.insert(
            "HTTPS_PROXY".to_string(),
            "http://127.0.0.1:54321/".to_string(),
        );
        true
    }

    /// ADR-F49 決定 19 (8c): この PC でサンドボックスの外に起動する Claude Code
    /// には、ACP のエージェントを外で起動するときと同じく egress のプロキシを
    /// 渡す。接続先 (この PC のループバックは届かない) と、インストールには渡さない。
    #[test]
    fn the_cli_started_outside_on_this_machine_is_pointed_at_the_proxy() {
        let launch = ClaudeCodeCommand::Launch { effort: None };
        let here = claude_code_spawn(&launch, None, false, test_proxy, None);
        assert_eq!(
            here.env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:54321/")
        );
        let on_host = claude_code_spawn(&launch, None, true, test_proxy, None);
        assert!(on_host.env.is_empty(), "{:?}", on_host.env);
        for on_remote_host in [false, true] {
            let install = claude_code_spawn(
                &ClaudeCodeCommand::Install,
                None,
                on_remote_host,
                test_proxy,
                None,
            );
            assert!(install.env.is_empty(), "{:?}", install.env);
        }
    }

    /// ADR-F53: タブをどこに開く経路でも、接続先では同じ包みを通る。組み立ては
    /// 1 つしかないので、エージェントパネルのタブ経路 (`claude_code_launch_task`)
    /// だけがログインシェルを通らない、ということが起きない。
    #[test]
    fn the_task_is_wrapped_for_the_host_whichever_tab_it_opens_in() {
        let launch = ClaudeCodeCommand::Launch {
            effort: Some("high".to_string()),
        };
        let here = claude_code_spawn(&launch, None, false, no_proxy, None);
        assert_eq!(here.command.as_deref(), Some(CLAUDE_CODE_PROGRAM));
        assert_eq!(here.args, ["--effort", "high"]);

        let on_host = claude_code_spawn(&launch, None, true, no_proxy, None);
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
        let install = claude_code_spawn(&ClaudeCodeCommand::Install, None, true, no_proxy, None);
        assert_eq!(install.command.as_deref(), Some(REMOTE_SHELL));
        assert_eq!(install.args.get(4).map(String::as_str), Some(NPM_PROGRAM));
    }

    fn ide_tools_for_test() -> ClaudeCodeIdeTools {
        ClaudeCodeIdeTools {
            args: vec![
                "--mcp-config".to_string(),
                r#"{"mcpServers":{"zed":{"type":"http","url":"http://127.0.0.1:4000/mcp/zed","headers":{"Authorization":"${ZED_IDE_BEARER}"}}}}"#.to_string(),
                "--allowedTools".to_string(),
                "mcp__zed__highlightCode,mcp__zed__getHover".to_string(),
            ],
            env: vec![("ZED_IDE_BEARER".to_string(), "Bearer secret-token".to_string())],
            keep_alive: None,
        }
    }

    /// ADR-F56: この PC で起動する CLI には IDE ツールの引数を足し、トークンは環境変数でだけ
    /// 渡す。引数はシェルがそのまま解釈するので、このシェル向けにクォートしてあり、読み戻すと
    /// 元の JSON (`${…}` はシェルではなく CLI が展開する) に戻る。
    #[test]
    fn a_host_launch_gets_the_ide_tools_without_the_token_in_its_arguments() {
        let launch = ClaudeCodeCommand::Launch {
            effort: Some("high".to_string()),
        };
        let ide_tools = ide_tools_for_test();
        let spawn = claude_code_spawn(&launch, None, false, no_proxy, Some(&ide_tools));

        assert_eq!(
            spawn.env.get("ZED_IDE_BEARER").map(String::as_str),
            Some("Bearer secret-token")
        );
        assert!(
            spawn
                .args
                .iter()
                .all(|argument| !argument.contains("secret-token")),
            "{:?}",
            spawn.args
        );
        assert!(!spawn.command_label.contains("secret-token"));
        assert_eq!(
            spawn.args.get(4..),
            Some(&["--effort".to_string(), "high".to_string()][..]),
            "the effort follows the IDE tools"
        );
        let shell_kind = task::Shell::System.shell_kind(cfg!(windows));
        let quoted = spawn
            .args
            .get(..4)
            .expect("the IDE tools' four arguments")
            .to_vec();
        for (quoted, original) in quoted.iter().zip(&ide_tools.args) {
            assert_eq!(
                shell_kind.try_quote(original).as_deref(),
                Some(quoted.as_str()),
                "each argument is quoted for the shell the launch goes through"
            );
        }
        if shell_kind == util::shell::ShellKind::Posix {
            let parsed = shlex::split(&quoted.join(" ")).expect("the shell reads it back");
            assert_eq!(parsed, ide_tools.args);
        }

        // 接続先 (SSH) のホストの CLI には渡さない: この PC の loopback には届かない。
        let on_host = claude_code_spawn(&launch, None, true, test_proxy, Some(&ide_tools));
        assert!(
            !on_host
                .args
                .iter()
                .any(|argument| argument.contains("mcp-config")),
            "{:?}",
            on_host.args
        );
        assert!(on_host.env.is_empty());

        // 8c 統合: プロキシの env と IDE ツールの env は両方入る (どちらも上書きしない)。
        let both = claude_code_spawn(&launch, None, false, test_proxy, Some(&ide_tools));
        assert_eq!(
            both.env.get("ZED_IDE_BEARER").map(String::as_str),
            Some("Bearer secret-token")
        );
        assert_eq!(
            both.env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:54321/")
        );

        let without = claude_code_spawn(&launch, None, false, no_proxy, None);
        assert_eq!(without.args, ["--effort", "high"]);
        assert!(without.env.is_empty());
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
            "before the deadline it is still waiting for the answer"
        );
        executor.advance_clock(Duration::from_secs(2));
        assert_eq!(
            message.await,
            None,
            "when it could not tell, it goes on to launch instead of stopping"
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
            "when it knows the program is missing, it stops with a message"
        );
    }

    /// The labels kept in tables and the texts that spell `${…}` go through the
    /// catalog at the point they are drawn, braces and all.
    #[test]
    fn table_labels_and_variable_hints_follow_the_catalog() {
        assert_eq!(timeout_choice_label(false), "Stop the Macro");
        assert_eq!(
            StepKind::SetVariable.text_placeholder(),
            "Value to set (${variable} allowed too)"
        );

        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(timeout_choice_label(false), "マクロを中止する");
        assert_eq!(timeout_choice_label(true), "次のステップを実行する");
        assert_eq!(
            StepKind::SetVariable.text_placeholder(),
            "入れる値 (${変数} も使えます)"
        );
        assert_eq!(
            StepKind::SetVariable.description(),
            "変数に値を入れます。後のステップで ${変数名} と書いたところがその値になります"
        );
        assert_eq!(
            macro_help()[2],
            "変数を使う: 送信する文字列・期待文字・判定する文字列・入れる値に ${名前} と書くと、\
             実行時にその値に置き換わります。${ を文字として送るときは $${ と書きます。"
        );
        assert_eq!(unfiled_folder(), "未分類");
    }

    /// 接続先に CLI が無いときは、こちらからインストールせず公式手順を案内する。
    #[test]
    fn the_missing_cli_message_names_where_to_install_it() {
        let on_host = claude_code_missing_program_message(CLAUDE_CODE_PROGRAM, true);
        assert!(
            on_host.contains("in a terminal on the remote host"),
            "{on_host}"
        );
        assert!(
            on_host.contains("https://claude.ai/install.sh"),
            "{on_host}"
        );
        assert!(on_host.contains("~/.local/bin/claude"), "{on_host}");
        let here = claude_code_missing_program_message(CLAUDE_CODE_PROGRAM, false);
        assert!(!here.contains("remote host"), "{here}");
        let npm_on_host = claude_code_missing_program_message(NPM_PROGRAM, true);
        assert!(npm_on_host.contains("npm"), "{npm_on_host}");
        assert!(npm_on_host.contains("remote host"), "{npm_on_host}");
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
