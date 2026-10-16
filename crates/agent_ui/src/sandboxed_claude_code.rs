//! Claude Code CLI の起動導線 (ADR-F49 決定 1・3・6、第 3 段、ADR-F53)。
//!
//! 「Claude Code を起動」は、
//! * Windows のローカルのプロジェクトでは、サンドボックス (WSL のヘルパー) の対話セッションとして
//!   CLI を起動し、IDE 連携はヘルパーの IDE 転送 → ブリッジ → サンドボックス用の IDE セッションで繋ぐ。
//! * SSH で繋いだ Linux のプロジェクトでは、接続先の remote_server をサンドボックスヘルパーとして
//!   同じ形で起動する (ADR-F18)。CLI は**この PC で取得・検証**してから接続先へ送る。IDE 連携も
//!   同じ形で繋ぐ (ADR-F53): ブリッジは接続先のヘルパーをもう一度 (`--ide-bridge` で) 起動した
//!   もので、パスの実体の確認はクライアントではなく接続先で行う (`ide_server`)。中継を起動でき
//!   なければ理由を出して IDE 連携なしで CLI を起動する。
//! * それ以外 (共同編集・対応しない接続方法・Windows 以外のローカル・フォルダーなし) と
//!   「サンドボックスの外で起動」は、その場の `claude` を起動のたびに確認してから起動する。
//!
//! ハンドラーが `terminal_view` ではなくここにあるのは、IDE セッション (`ide_server`) と Linux 版の
//! CLI の取得 (F49-L1) を使うため。`terminal_view` はどちらにも依存できない (`agent_ui` が
//! `terminal_view` に依存している)。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fs::Fs;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, PromptButton, PromptLevel,
    SharedString, Task, WeakEntity, Window,
};
use project::Project;
use task::{Shell, SpawnInTerminal};
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::prepare_task_for_spawn;
use util::ResultExt as _;
use workspace::Workspace;
use workspace::workspace_error::{ErrorAction, ErrorSeverity, WorkspaceError};
use zed_actions::terminal::{LaunchClaudeCode, LaunchClaudeCodeOutsideSandbox};

use crate::AgentPanel;
use crate::ai_choices::AiPlacement;

pub(crate) use session::cli_tab_session_key;

/// The program the launch actions start.
const CLAUDE_PROGRAM: &str = "claude";
const OUTSIDE_SANDBOX_MESSAGE: &str = i18n::msg!("Start Claude Code outside the sandbox?");
/// ADR-F46 A-3 の帯と同じ言い回し。
const OUTSIDE_SANDBOX_NOTICE: &str =
    i18n::msg!("Runs outside the sandbox (with Zed's own permissions)");
/// 外で失うもの。通信はこの PC のプロキシを指す環境変数を渡すだけで、読まない
/// プログラムには効かない (ADR-F13 の協力方式) ので「強制ではない」と言い切る。
const OUTSIDE_SANDBOX_LIMITS_LOCAL: &str = i18n::msg!(
    "Writes are no longer restricted. Network restrictions only apply to programs that read the proxy settings (they are not enforced)."
);
/// 接続先のプロセスには、この PC のプロキシも許可リストも届かない。
const OUTSIDE_SANDBOX_LIMITS_REMOTE: &str = i18n::msg!(
    "Writes and network access are not restricted (Zed's controls do not apply on the remote host)."
);
const OUTSIDE_SANDBOX_CLAUDE_LOGIN: &str =
    i18n::msg!("The sign-in is shared with your usual Claude Code (`~/.claude`).");
const OUTSIDE_SANDBOX_OTHER_LOGIN: &str =
    i18n::msg!("Your usual settings and sign-in are used as they are.");
const OUTSIDE_SANDBOX_ONCE: &str = i18n::msg!(
    "This applies to this start only. To always start outside the sandbox, switch it on the AI page in Settings."
);
/// 設定フォルダーが中 (`config`) と外 (`~/.claude` など) で違うので、動いている
/// スレッドを外へ移すと会話の続きは読み込めない。
const OUTSIDE_SANDBOX_NEW_SESSION: &str =
    i18n::msg!("This starts a new session (it does not continue this conversation).");
const LAUNCH_ONCE_LABEL: &str = i18n::msg!("Start (This Time Only)");
/// ACP のスレッドの同意は記録され、そのスレッドは次から聞かずに外で起動する。
const OUTSIDE_SANDBOX_THIS_THREAD: &str = i18n::msg!(
    "This applies to this thread only (you will not be asked again; you can revoke the consent on the AI page in Settings). To always start outside the sandbox, switch it on the AI page in Settings."
);
pub(crate) const LAUNCH_FOR_THIS_THREAD_LABEL: &str = i18n::msg!("Start (This Thread Only)");
const CANCEL_LABEL: &str = i18n::msg!("Cancel");
/// サンドボックスでの起動に失敗した通知のボタン。押すと「外で起動」の確認に進む。
const LAUNCH_OUTSIDE_AFTER_FAILURE_LABEL: &str = i18n::msg!("Start Outside the Sandbox…");

const COLLABORATION_REASON: &str = i18n::msg!(
    "The sandboxed Claude Code is not available in a project that is being edited collaboratively."
);
/// SSH 以外の接続 (WSL リモート接続・Docker など)。置き場も待ち合わせもそれぞれ違うので、
/// この段では外へ回す。
const REMOTE_CONNECTION_REASON: &str =
    i18n::msg!("The sandboxed Claude Code is not available yet for this type of connection.");
const REMOTE_NOT_LINUX_REASON: &str = i18n::msg!(
    "The sandboxed Claude Code is not available because the remote host is not running Linux."
);
const UNSUPPORTED_OS_REASON: &str =
    i18n::msg!("The sandboxed Claude Code is not available yet on this OS.");
const NO_FOLDER_REASON: &str =
    i18n::msg!("The sandboxed Claude Code is not available because no folder is open.");

/// Starts the CLI outside the sandbox, once the user has confirmed it. A
/// parameter so that tests can observe the launch without a real terminal.
type HostLauncher =
    Rc<dyn Fn(&mut Workspace, Option<String>, &mut Window, &mut Context<Workspace>)>;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        register_launch_actions(workspace, Rc::new(launch_on_host));
    })
    .detach();
}

fn register_launch_actions(workspace: &mut Workspace, launch_on_host: HostLauncher) {
    workspace.register_action({
        let launch_on_host = launch_on_host.clone();
        move |workspace, action: &LaunchClaudeCode, window, cx| {
            launch_claude_code(
                workspace,
                action.effort.clone(),
                launch_on_host.clone(),
                window,
                cx,
            )
        }
    });
    workspace.register_action(
        move |workspace, action: &LaunchClaudeCodeOutsideSandbox, window, cx| {
            launch_claude_code_outside_sandbox(
                workspace,
                action.effort.clone(),
                launch_on_host.clone(),
                window,
                cx,
            )
        },
    );
}

/// Starts Claude Code in `workspace` as its [`LaunchClaudeCode`] (or, with
/// `outside_sandbox`, [`LaunchClaudeCodeOutsideSandbox`]) would. For menus
/// that name their project (the sidebar's): an action dispatched in the
/// window reaches the workspace the window last drew, which may be another.
pub(crate) fn launch_claude_code_in(
    workspace: &Entity<Workspace>,
    outside_sandbox: bool,
    window: &mut Window,
    cx: &mut App,
) {
    workspace.update(cx, |workspace, cx| {
        let launch_on_host: HostLauncher = Rc::new(launch_on_host);
        if outside_sandbox {
            launch_claude_code_outside_sandbox(workspace, None, launch_on_host, window, cx)
        } else {
            launch_claude_code(workspace, None, launch_on_host, window, cx)
        }
    })
}

fn launch_claude_code(
    workspace: &mut Workspace,
    effort: Option<String>,
    launch_on_host: HostLauncher,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().read(cx);
    let shape = ProjectShape::of(project, cx);
    let on_remote_host = project.is_via_remote_server();
    match route_for(shape) {
        #[cfg(target_os = "windows")]
        LaunchRoute::LocalSandbox => launch::launch_in_sandbox(workspace, effort, window, cx),
        #[cfg(not(target_os = "windows"))]
        LaunchRoute::LocalSandbox => confirm_and_launch_outside_sandbox(
            effort,
            Some(UNSUPPORTED_OS_REASON),
            on_remote_host,
            launch_on_host,
            window,
            cx,
        ),
        LaunchRoute::RemoteSandbox => {
            remote_launch::launch_on_remote_host(workspace, effort, window, cx)
        }
        LaunchRoute::OutsideSandbox { reason } => confirm_and_launch_outside_sandbox(
            effort,
            Some(reason),
            on_remote_host,
            launch_on_host,
            window,
            cx,
        ),
        LaunchRoute::OutsideSandboxByUserChoice => launch_on_host(workspace, effort, window, cx),
    }
}

fn launch_claude_code_outside_sandbox(
    workspace: &mut Workspace,
    effort: Option<String>,
    launch_on_host: HostLauncher,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // The user has already said yes to every start in their own
    // settings file, so asking again would only be noise.
    if project::agent_sandbox::claude_code_runs_outside_sandbox_by_user_choice(cx) {
        launch_on_host(workspace, effort, window, cx);
        return;
    }
    let on_remote_host = workspace.project().read(cx).is_via_remote_server();
    confirm_and_launch_outside_sandbox(effort, None, on_remote_host, launch_on_host, window, cx);
}

/// What decides where the CLI can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProjectShape {
    /// Seen by collaborators: the sandbox's IDE session serves only a project
    /// nobody else sees.
    is_collaborative: bool,
    /// `None` for a local project; otherwise how it is connected.
    remote: Option<RemoteShape>,
    has_folder: bool,
    sandbox_platform: bool,
    /// `agent.claude_code_run_outside_sandbox`, from the user's own settings
    /// file only (a project's settings cannot let the CLI out).
    outside_by_user_choice: bool,
}

/// The connection of a remote project, as the sandbox cares about it: only an
/// SSH host runs the helper this launcher knows how to drive (ADR-F18), and
/// only a Linux one has the helper at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RemoteShape {
    is_ssh: bool,
    is_linux: bool,
}

impl ProjectShape {
    fn of(project: &Project, cx: &App) -> Self {
        Self {
            is_collaborative: project.is_via_collab() || project.remote_id().is_some(),
            remote: project.remote_client().map(|remote_client| {
                let remote_client = remote_client.read(cx);
                RemoteShape {
                    is_ssh: remote_client.connection_type() == "ssh",
                    is_linux: remote_client.remote_platform().os == remote::RemoteOs::Linux,
                }
            }),
            has_folder: !sandbox_roots(project, cx).is_empty(),
            sandbox_platform: cfg!(target_os = "windows"),
            outside_by_user_choice:
                project::agent_sandbox::claude_code_runs_outside_sandbox_by_user_choice(cx),
        }
    }
}

/// ADR-F94: where "Launch Claude Code" would start the CLI in `project`, as the
/// "Choose AI" menu shows it. Read from the same route the launch takes.
pub(crate) fn claude_code_placement(project: &Project, cx: &App) -> AiPlacement {
    placement_of_route(route_for(ProjectShape::of(project, cx)))
}

fn placement_of_route(route: LaunchRoute) -> AiPlacement {
    match route {
        #[cfg(target_os = "windows")]
        LaunchRoute::LocalSandbox => AiPlacement::Sandbox,
        // Off Windows the local route still asks before starting outside.
        #[cfg(not(target_os = "windows"))]
        LaunchRoute::LocalSandbox => AiPlacement::OutsideAfterConfirmation,
        LaunchRoute::RemoteSandbox => AiPlacement::Sandbox,
        LaunchRoute::OutsideSandbox { .. } => AiPlacement::OutsideAfterConfirmation,
        LaunchRoute::OutsideSandboxByUserChoice => AiPlacement::OutsideByChoice,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchRoute {
    /// This machine's WSL sandbox.
    LocalSandbox,
    /// The sandbox helper of the SSH host the project is on.
    RemoteSandbox,
    /// Always confirmed first, with `reason` saying why it is not the sandbox.
    OutsideSandbox { reason: &'static str },
    /// Outside the sandbox without asking: the user chose that for every
    /// start in their own settings file.
    OutsideSandboxByUserChoice,
}

fn route_for(shape: ProjectShape) -> LaunchRoute {
    if shape.outside_by_user_choice {
        return LaunchRoute::OutsideSandboxByUserChoice;
    }
    let reason = if shape.is_collaborative {
        COLLABORATION_REASON
    } else if let Some(remote) = shape.remote {
        // The client's own OS does not matter here: the sandbox is the host's.
        if !remote.is_ssh {
            REMOTE_CONNECTION_REASON
        } else if !remote.is_linux {
            REMOTE_NOT_LINUX_REASON
        } else if !shape.has_folder {
            NO_FOLDER_REASON
        } else {
            return LaunchRoute::RemoteSandbox;
        }
    } else if !shape.sandbox_platform {
        UNSUPPORTED_OS_REASON
    } else if !shape.has_folder {
        NO_FOLDER_REASON
    } else {
        return LaunchRoute::LocalSandbox;
    };
    LaunchRoute::OutsideSandbox { reason }
}

/// The folders a sandboxed session works in: the visible worktrees that are
/// directories. They are both what it may write and the IDE session's roots,
/// which have to be visible worktrees.
fn sandbox_roots(project: &Project, cx: &App) -> Vec<PathBuf> {
    project
        .visible_worktrees(cx)
        .filter_map(|worktree| {
            let worktree = worktree.read(cx);
            worktree
                .root_entry()?
                .is_dir()
                .then(|| worktree.abs_path().to_path_buf())
        })
        .collect()
}

/// The confirmation's question, naming what starts: Claude Code, or the agent
/// CLI an agent panel terminal's init command starts.
fn outside_sandbox_message(program: &str) -> String {
    if program == CLAUDE_PROGRAM {
        i18n::tr(OUTSIDE_SANDBOX_MESSAGE).to_string()
    } else {
        i18n::t!("Start `{program}` outside the sandbox?")
    }
}

/// What starting outside the sandbox is, beyond where it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutsideSandboxStart {
    /// A start of its own: the launch actions.
    Once,
    /// An agent's thread started outside its sandbox from its card. The
    /// consent is written down for the thread (td).
    Thread,
    /// A connected thread started again outside its sandbox, whose session
    /// cannot follow it there.
    RestartingThread,
}

impl OutsideSandboxStart {
    fn is_kept_for_the_thread(self) -> bool {
        match self {
            Self::Once => false,
            Self::Thread | Self::RestartingThread => true,
        }
    }
}

/// On a remote project the CLI is the host's and runs with that user's
/// rights, neither this PC's nor Zed's.
///
/// The three lines after the headline are what the user gives up and keeps:
/// the limits that go, the sign-in that is shared (outside the sandbox the
/// agent reads the account's own folder, so a folder of the fork's would
/// guard nothing — ADR-F55), and that this is for this start only.
fn outside_sandbox_detail(
    program: &str,
    on_remote_host: bool,
    reason: Option<&str>,
    start: OutsideSandboxStart,
) -> String {
    let mut lines = vec![if on_remote_host {
        i18n::t!(
            "Runs `{program}` on the remote host with that host user's permissions (outside the sandbox)."
        )
    } else {
        i18n::t!(
            "{notice}. Starts `{program}` on this PC in the project folder.",
            notice = i18n::tr(OUTSIDE_SANDBOX_NOTICE)
        )
    }];
    if let Some(reason) = reason {
        lines.push(i18n::tr(reason).to_string());
    }
    lines.push(
        i18n::tr(if on_remote_host {
            OUTSIDE_SANDBOX_LIMITS_REMOTE
        } else {
            OUTSIDE_SANDBOX_LIMITS_LOCAL
        })
        .to_string(),
    );
    lines.push(
        i18n::tr(
            if program == CLAUDE_PROGRAM || program == agent_servers::CLAUDE_AGENT_ID {
                OUTSIDE_SANDBOX_CLAUDE_LOGIN
            } else {
                OUTSIDE_SANDBOX_OTHER_LOGIN
            },
        )
        .to_string(),
    );
    lines.push(
        i18n::tr(if start.is_kept_for_the_thread() {
            OUTSIDE_SANDBOX_THIS_THREAD
        } else {
            OUTSIDE_SANDBOX_ONCE
        })
        .to_string(),
    );
    if start == OutsideSandboxStart::RestartingThread {
        lines.push(i18n::tr(OUTSIDE_SANDBOX_NEW_SESSION).to_string());
    }
    lines.join("\n")
}

/// Asks, every time and with no "always", before the agent CLI `program`
/// runs outside the sandbox: there it runs the project's hooks and MCP
/// servers (`.claude/`, `.mcp.json`, …) with the rights of whoever runs it.
/// Resolves to whether the user agreed.
pub(crate) fn confirm_running_outside_sandbox(
    program: &str,
    on_remote_host: bool,
    reason: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> impl Future<Output = bool> + use<> {
    confirm_starting_outside_sandbox(
        program,
        on_remote_host,
        reason,
        OutsideSandboxStart::Once,
        window,
        cx,
    )
}

/// [`confirm_running_outside_sandbox`] for a start that is not a plain one.
///
/// ADR-F92: while running outside the sandbox is not allowed, nothing is
/// asked: the user is told so, with the way to the setting, and the answer is
/// no.
pub(crate) fn confirm_starting_outside_sandbox(
    program: &str,
    on_remote_host: bool,
    reason: Option<&str>,
    start: OutsideSandboxStart,
    window: &mut Window,
    cx: &mut App,
) -> impl Future<Output = bool> + use<> {
    let answer = if project::agent_sandbox::outside_sandbox_allowed(cx) {
        Some(ask_to_start_outside_sandbox(
            program,
            on_remote_host,
            reason,
            start,
            window,
            cx,
        ))
    } else {
        show_outside_sandbox_refusal(window, cx);
        None
    };
    async move {
        match answer {
            Some(answer) => answer.await,
            None => false,
        }
    }
}

/// ADR-F96: [`confirm_starting_outside_sandbox`] for the chat input's sandbox
/// button. Where running outside the sandbox is not allowed, the same question
/// also asks to turn that on (ADR-F92), and the answer is yes only once the
/// settings have it: one click, one dialog, rather than one dialog for the
/// setting and another for the start.
pub(crate) fn confirm_starting_outside_sandbox_allowing_it(
    program: &str,
    on_remote_host: bool,
    start: OutsideSandboxStart,
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    if project::agent_sandbox::outside_sandbox_allowed(cx) {
        let answer = ask_to_start_outside_sandbox(program, on_remote_host, None, start, window, cx);
        return window.spawn(cx, async move |_| answer.await);
    }
    let message = outside_sandbox_message(program);
    let detail = format!(
        "{}\n{}",
        outside_sandbox_detail(program, on_remote_host, None, start),
        i18n::t!(
            "This also turns on \"Allow Running Outside the Sandbox\" (Settings > AI > Sandbox), \
             which is off. It stays on until you turn it off there."
        )
    );
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some(&detail),
        &[
            PromptButton::ok(i18n::t!("Turn On and Start Outside")),
            PromptButton::cancel(i18n::tr(CANCEL_LABEL)),
        ],
        cx,
    );
    window.spawn(cx, async move |cx| {
        if !answer.await.is_ok_and(|index| index == 0) {
            return false;
        }
        let Ok(turned_on) = cx.update(|window, cx| turn_on_outside_sandbox(fs, window, cx)) else {
            return false;
        };
        turned_on.await
    })
}

/// ADR-F92: the title of the notice that a start outside the sandbox was
/// refused, and of the cards that say so.
pub(crate) const OUTSIDE_SANDBOX_NOT_ALLOWED_TITLE: &str =
    i18n::msg!("Running Outside the Sandbox Is Not Allowed");
pub(crate) const OUTSIDE_SANDBOX_NOT_ALLOWED_DETAIL: &str = i18n::msg!(
    "\"Allow Running Outside the Sandbox\" is off in Settings > AI > Sandbox, so nothing starts outside the sandbox, and the settings that always start an agent or Claude Code outside it are ignored. To allow it, turn that setting on."
);
pub(crate) const OPEN_SANDBOX_SETTINGS_LABEL: &str = i18n::msg!("Open Sandbox Settings");
const CLOSE_LABEL: &str = i18n::msg!("Close");

/// ADR-F92: says that running outside the sandbox is not allowed, offering
/// to open the setting that allows it. Not a question: nothing runs either
/// way.
pub(crate) fn show_outside_sandbox_refusal(window: &mut Window, cx: &mut App) {
    let answer = window.prompt(
        PromptLevel::Info,
        i18n::tr(OUTSIDE_SANDBOX_NOT_ALLOWED_TITLE),
        Some(i18n::tr(OUTSIDE_SANDBOX_NOT_ALLOWED_DETAIL)),
        &[
            PromptButton::ok(i18n::tr(OPEN_SANDBOX_SETTINGS_LABEL)),
            PromptButton::cancel(i18n::tr(CLOSE_LABEL)),
        ],
        cx,
    );
    window
        .spawn(cx, async move |cx| {
            if answer.await.is_ok_and(|index| index == 0) {
                cx.update(|window, cx| open_sandbox_settings(window, cx))
                    .log_err();
            }
        })
        .detach();
}

pub(crate) fn open_sandbox_settings(window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(zed_actions::OpenSettingsAt {
            path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
            target: None,
        }),
        cx,
    );
}

/// ADR-F96: before the chat input's sandbox button turns the sandbox off,
/// asks to turn on "Allow Running Outside the Sandbox" (ADR-F92) where it is
/// off, writes it to the user's settings file, and resolves once the settings
/// have it. False where the user declined, or the settings never came to
/// allow it (a settings profile or the global settings file can keep it off),
/// which the refusal then says.
pub(crate) fn allow_outside_sandbox_first(
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    if project::agent_sandbox::outside_sandbox_allowed(cx) {
        return Task::ready(true);
    }
    let answer = window.prompt(
        PromptLevel::Warning,
        i18n::t!("Allow Running Outside the Sandbox?"),
        Some(i18n::t!(
            "Turning the sandbox off needs \"Allow Running Outside the Sandbox\" (Settings > AI > \
             Sandbox), which is off. Turn it on? It stays on until you turn it off there."
        )),
        &[
            PromptButton::ok(i18n::t!("Turn On and Continue")),
            PromptButton::cancel(i18n::t!("Cancel")),
        ],
        cx,
    );
    window.spawn(cx, async move |cx| {
        if !answer.await.is_ok_and(|index| index == 0) {
            return false;
        }
        let Ok(turned_on) = cx.update(|window, cx| turn_on_outside_sandbox(fs, window, cx)) else {
            return false;
        };
        turned_on.await
    })
}

/// ADR-F96: writes "Allow Running Outside the Sandbox" (ADR-F92) to the user's
/// settings file, after the user agreed to that, and resolves once the
/// settings have it. False where they never came to allow it (a settings
/// profile or the global settings file can keep it off), which the refusal
/// then says.
fn turn_on_outside_sandbox(fs: Arc<dyn Fs>, window: &mut Window, cx: &mut App) -> Task<bool> {
    settings::update_settings_file(fs, cx, |settings, _| {
        settings
            .agent
            .get_or_insert_default()
            .sandbox_permissions
            .get_or_insert_default()
            .allow_outside_sandbox = Some(true);
    });
    window.spawn(cx, async move |cx| {
        // The settings store takes the file in when its watcher sees the write.
        for _ in 0..50 {
            if cx
                .update(|_, cx| project::agent_sandbox::outside_sandbox_allowed(cx))
                .unwrap_or(false)
            {
                return true;
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
        cx.update(|window, cx| show_outside_sandbox_refusal(window, cx))
            .log_err();
        false
    })
}

/// ADR-F96 stage 2: before the chat input's network button lifts a thread's
/// network restriction, asks to turn on
/// `agent.sandbox_permissions.allow_unrestricted_network` where it is off,
/// writes it to the user's settings file, and resolves once the settings have
/// it, the same way as [`allow_outside_sandbox_first`]. False where the user
/// declined, or the settings never came to allow it.
pub(crate) fn allow_unrestricted_network_first(
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    ask_to_allow_unrestricted_network(
        i18n::t!(
            "Letting this thread's terminal commands and fetches reach any host needs \
             \"Allow Lifting the Network Restriction for a Thread\" (Settings > AI > Sandbox), \
             which is off. Turn it on? It stays on until you turn it off there. The network \
             allowlist still applies."
        ),
        fs,
        window,
        cx,
    )
}

/// ADR-F96 stage 3: [`allow_unrestricted_network_first`] for an external
/// agent, whose whole process starts again rather than its commands and
/// fetches being let out.
pub(crate) fn allow_unrestricted_network_first_for_external_agent(
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    ask_to_allow_unrestricted_network(
        i18n::t!(
            "Letting this thread's agent and the commands it runs reach any host within the \
             network allowlist needs \"Allow Lifting the Network Restriction for a Thread\" \
             (Settings > AI > Sandbox), which is off. Turn it on? It stays on until you turn it \
             off there."
        ),
        fs,
        window,
        cx,
    )
}

fn ask_to_allow_unrestricted_network(
    detail: &'static str,
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    if project::agent_sandbox::unrestricted_network_allowed(cx) {
        return Task::ready(true);
    }
    let answer = window.prompt(
        PromptLevel::Warning,
        i18n::t!("Allow Lifting the Network Restriction?"),
        Some(detail),
        &[
            PromptButton::ok(i18n::t!("Turn On and Continue")),
            PromptButton::cancel(i18n::t!("Cancel")),
        ],
        cx,
    );
    window.spawn(cx, async move |cx| {
        if !answer.await.is_ok_and(|index| index == 0) {
            return false;
        }
        let Ok(turned_on) = cx.update(|window, cx| turn_on_unrestricted_network(fs, window, cx))
        else {
            return false;
        };
        turned_on.await
    })
}

/// ADR-F96 stage 2: writes `allow_unrestricted_network` to the user's settings
/// file, after the user agreed to that, and resolves once the settings have it,
/// as [`turn_on_outside_sandbox`] does. False where they never came to allow
/// it, which a notice then says.
pub(crate) fn turn_on_unrestricted_network(
    fs: Arc<dyn Fs>,
    window: &mut Window,
    cx: &mut App,
) -> Task<bool> {
    settings::update_settings_file(fs, cx, |settings, _| {
        settings
            .agent
            .get_or_insert_default()
            .sandbox_permissions
            .get_or_insert_default()
            .allow_unrestricted_network = Some(true);
    });
    window.spawn(cx, async move |cx| {
        for _ in 0..50 {
            if cx
                .update(|_, cx| project::agent_sandbox::unrestricted_network_allowed(cx))
                .unwrap_or(false)
            {
                return true;
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
        cx.update(|window, cx| {
            let closed = window.prompt(
                PromptLevel::Info,
                i18n::t!("The Network Restriction Stays On"),
                Some(i18n::t!(
                    "`allow_unrestricted_network` did not take effect. A settings profile or the \
                     global settings file may keep it off."
                )),
                &[PromptButton::ok(i18n::t!("Close"))],
                cx,
            );
            window
                .spawn(cx, async move |_| {
                    closed.await.ok();
                })
                .detach();
        })
        .log_err();
        false
    })
}

fn ask_to_start_outside_sandbox(
    program: &str,
    on_remote_host: bool,
    reason: Option<&str>,
    start: OutsideSandboxStart,
    window: &mut Window,
    cx: &mut App,
) -> impl Future<Output = bool> + use<> {
    let message = outside_sandbox_message(program);
    let detail = outside_sandbox_detail(program, on_remote_host, reason, start);
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some(&detail),
        &[
            PromptButton::ok(i18n::tr(if start.is_kept_for_the_thread() {
                LAUNCH_FOR_THIS_THREAD_LABEL
            } else {
                LAUNCH_ONCE_LABEL
            })),
            PromptButton::cancel(i18n::tr(CANCEL_LABEL)),
        ],
        cx,
    );
    async move { answer.await.is_ok_and(|index| index == 0) }
}

fn confirm_and_launch_outside_sandbox(
    effort: Option<String>,
    reason: Option<&'static str>,
    on_remote_host: bool,
    launch_on_host: HostLauncher,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let confirmed =
        confirm_running_outside_sandbox(CLAUDE_PROGRAM, on_remote_host, reason, window, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        if !confirmed.await {
            return;
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                launch_on_host(workspace, effort, window, cx)
            })
            .log_err();
    })
    .detach();
}

/// A sandboxed launch that did not come up (ADR-F49 決定 19). The notice
/// offers the start outside the sandbox the user may want instead; the button
/// only dispatches [`LaunchClaudeCodeOutsideSandbox`], so that start is still
/// confirmed first.
struct SandboxLaunchFailure {
    message: String,
    effort: Option<String>,
    outside_sandbox_allowed: bool,
}

impl WorkspaceError for SandboxLaunchFailure {
    fn primary_message(&self) -> SharedString {
        self.message.clone().into()
    }

    fn primary_action(&self) -> ErrorAction {
        // ADR-F92: where starting outside is not allowed, the way there is the
        // setting rather than a start that would only be refused.
        if !self.outside_sandbox_allowed {
            return ErrorAction::new(
                i18n::tr(OPEN_SANDBOX_SETTINGS_LABEL),
                zed_actions::OpenSettingsAt {
                    path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                    target: None,
                },
            );
        }
        ErrorAction::new(
            i18n::tr(LAUNCH_OUTSIDE_AFTER_FAILURE_LABEL),
            LaunchClaudeCodeOutsideSandbox {
                effort: self.effort.clone(),
            },
        )
    }

    fn secondary_action(&self) -> Option<ErrorAction> {
        Some(ErrorAction::dismiss())
    }

    fn severity(&self) -> ErrorSeverity {
        ErrorSeverity::Critical
    }
}

fn show_launch_failure(
    workspace: &mut Workspace,
    message: String,
    effort: Option<String>,
    cx: &mut Context<Workspace>,
) {
    let outside_sandbox_allowed = project::agent_sandbox::outside_sandbox_allowed(cx);
    workspace.show_error(
        SandboxLaunchFailure {
            message,
            effort,
            outside_sandbox_allowed,
        },
        cx,
    );
}

/// CLI のタブを置けるエージェントパネル。置けないときは `None` で、呼び出し元は
/// 今までどおりターミナルパネルに開く (ADR-F53 の置き場の規則)。
///
/// パネルが登録されていても、AI を有効にしていない (`agent.enabled = false` =
/// このフォークの既定、ADR-F09) ときは「無い」ものとして扱う。パネルが外れるのは
/// `disable_ai` だけなので、既定のままの利用者にもパネルは登録されている: そこへ
/// CLI のタブを置くと、opt-in していない AI のパネルが勝手に前に出たうえ、タブを
/// 閉じたときの `AgentPanel::activate_draft` が下書きのスレッドを作り、その
/// `AgentConnectionStore::request_connection` がエージェントへの接続まで始めて
/// しまう。CLI 自体は人間の操作 (ADR-F38) なので AI が無効でも使える。
///
/// ターミナルを開けないプロジェクト (共同編集のゲスト) も `None`: 元の経路
/// ([`terminal_view::terminal_panel::TerminalPanel::spawn_task`]) はそこで断る
/// 文言を持っているので、判断ごとそちらに任せる。
pub(crate) fn agent_panel_for_cli_tab(
    workspace: &Workspace,
    cx: &App,
) -> Option<Entity<AgentPanel>> {
    if !workspace.project().read(cx).supports_terminal(cx) {
        return None;
    }
    let panel = workspace.panel::<AgentPanel>(cx)?;
    workspace::Panel::enabled(panel.read(cx), cx).then_some(panel)
}

/// セッションの端末の作り方。ADR-F53 で変えたのはタブの置き場だけなので、
/// 経路ごとの実行のしかたはここで区別してそのまま保つ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SessionTerminalKind {
    /// サンドボックスのラッパーをそのまま実行する。ラッパーより手前で走る
    /// 端末側の環境 (ディレクトリ環境・`terminal.env`・ツールチェーン) は
    /// 足さない ([`Project::create_sandboxed_terminal_task`])。
    Sandboxed,
    /// この PC の `claude`。人がターミナルに打つのと同じように、プロジェクトの
    /// シェルで包む ([`prepare_task_for_spawn`] = ターミナルパネル経路と同じ)。
    Host,
}

/// 組み立てたタスクを**エージェントパネルのタブ**として開く (ADR-F53)。
///
/// ワークスペースとパネルはそれぞれ別の `update` で触る (パネルの更新中に
/// ワークスペースを読むと二重リースで落ちる)。
async fn open_terminal_tab_in_agent_panel(
    workspace: &WeakEntity<Workspace>,
    panel: Entity<AgentPanel>,
    spawn_task: SpawnInTerminal,
    kind: SessionTerminalKind,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Terminal>> {
    let (project, remote_shell, is_windows) = workspace.read_with(cx, |workspace, cx| {
        let project = workspace.project().clone();
        let project_ref = project.read(cx);
        let remote_shell = project_ref
            .remote_client()
            .and_then(|remote_client| remote_client.read(cx).shell());
        let is_windows = project_ref.path_style(cx).is_windows();
        (project, remote_shell, is_windows)
    })?;

    let spawn_task = match kind {
        SessionTerminalKind::Sandboxed => spawn_task,
        SessionTerminalKind::Host => {
            let shell = match remote_shell {
                Some(remote_shell) if spawn_task.shell == Shell::System => {
                    Shell::Program(remote_shell)
                }
                _ => spawn_task.shell.clone(),
            };
            prepare_task_for_spawn(&spawn_task, &shell, is_windows)
        }
    };

    let terminal = project
        .update(cx, |project, cx| match kind {
            SessionTerminalKind::Sandboxed => {
                project.create_sandboxed_terminal_task(spawn_task, cx)
            }
            SessionTerminalKind::Host => project.create_terminal_task(spawn_task, cx),
        })
        .await?;

    let (terminal_view, focus) = workspace.update_in(cx, |workspace, window, cx| {
        let workspace_handle = workspace.weak_handle();
        let workspace_id = workspace.database_id();
        let project = workspace.project().downgrade();
        let terminal = terminal.clone();
        let view = cx.new(|cx| {
            TerminalView::new(
                terminal,
                workspace_handle,
                workspace_id,
                project,
                window,
                cx,
            )
        });
        // 端末が立ち上がるまでに何かのモーダルが開いていたら、そちらへの
        // キー入力を奪わずタブを出すだけにする (`add_terminal_with` と同じ判断)。
        (view, !workspace.has_active_modal(window, cx))
    })?;

    panel.update_in(cx, |panel, window, cx| {
        panel.add_terminal_tab(terminal_view, focus, window, cx);
    })?;
    workspace.update_in(cx, |workspace, window, cx| {
        if focus {
            workspace.focus_panel::<AgentPanel>(window, cx);
        } else {
            workspace.open_panel::<AgentPanel>(window, cx);
        }
    })?;

    Ok(terminal)
}

/// 公式 CLI をこの PC で (サンドボックスの外で) 起動する。置き場はサンドボックス
/// 経路と同じ規則: エージェントパネルがあればそのタブ、無ければターミナル
/// パネル (ADR-F53)。プログラム・引数・環境・案内の中身は `terminal_view` 側の
/// 組み立てをそのまま使う。
fn launch_on_host(
    workspace: &mut Workspace,
    effort: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    launch_on_host_in_tab(
        workspace,
        effort,
        &terminal_view::session_tools::spawn_claude_code_on_host,
        window,
        cx,
    );
}

/// The label an agent's highlights and annotations are framed with.
const CLAUDE_CODE_ASSIST_LABEL: &str = "Claude Code";

/// ADR-F56: Zed's IDE tools for a CLI started on this machine: the `zed` MCP
/// server on the host's IDE server (started for it when `ide_server.enabled`
/// is off), its token in the environment only. `None` on a remote project,
/// whose CLI runs on the host and cannot reach this machine's loopback, and
/// when `ide_server.assist_tools` is off.
fn host_ide_tools(
    workspace: &Workspace,
    cx: &mut App,
) -> Option<terminal_view::session_tools::ClaudeCodeIdeTools> {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() || !ide_server::assist_tools_enabled(cx) {
        return None;
    }
    let client = ide_server::host_assist_client(&project, CLAUDE_CODE_ASSIST_LABEL, cx)?;
    let endpoint = client.assist_endpoint(cx)?;
    Some(terminal_view::session_tools::ClaudeCodeIdeTools {
        args: ide_server::cli_assist_arguments(&endpoint.url),
        env: vec![(
            ide_server::ASSIST_BEARER_ENV_VAR.to_string(),
            endpoint.authorization,
        )],
        keep_alive: Some(Box::new(client)),
    })
}

/// [`launch_on_host`] の中身。エージェントパネルが無いときの置き場
/// (`without_agent_panel`) を差し替えられるのはテストのため。
fn launch_on_host_in_tab(
    workspace: &mut Workspace,
    effort: Option<String>,
    without_agent_panel: &dyn Fn(
        &mut Workspace,
        Option<String>,
        &mut Window,
        &mut Context<Workspace>,
        Option<terminal_view::session_tools::ClaudeCodeIdeTools>,
    ),
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ide_tools = host_ide_tools(workspace, cx);
    let Some(panel) = agent_panel_for_cli_tab(workspace, cx) else {
        without_agent_panel(workspace, effort, window, cx, ide_tools);
        return;
    };

    let spawn_task = terminal_view::session_tools::claude_code_launch_task(
        effort,
        workspace,
        cx,
        ide_tools.as_ref(),
    );
    let keep_alive = ide_tools.and_then(|ide_tools| ide_tools.keep_alive);
    // この PC なら `which`、接続先なら `command -v` を 1 往復 — どちらの案内文も
    // ターミナルパネル経路と同じものを使う (ADR-F53)。
    let lookup = terminal_view::session_tools::claude_code_launch_lookup(workspace, cx);

    cx.spawn_in(window, async move |workspace, cx| {
        let executor = cx.background_executor().clone();
        if let Some(message) = lookup.missing_message(&executor).await {
            workspace
                .update(cx, |workspace, cx| workspace.show_error(message, cx))
                .log_err();
            return;
        }

        match open_terminal_tab_in_agent_panel(
            &workspace,
            panel,
            spawn_task,
            SessionTerminalKind::Host,
            cx,
        )
        .await
        {
            Ok(terminal) => {
                // ADR-F56: the IDE server keeps this CLI's registration (and what
                // it drew) until the CLI exits.
                if let Some(keep_alive) = keep_alive
                    && let Ok(completion) =
                        cx.update(|_, cx| terminal.read(cx).wait_for_completed_task(cx))
                {
                    completion.await;
                    drop(keep_alive);
                }
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.show_error(
                            i18n::t!("Could not run the command in the terminal: {error:#}"),
                            cx,
                        )
                    })
                    .log_err();
            }
        }
    })
    .detach();
}

/// The parts of a sandboxed session that do not depend on which Linux runs it:
/// where its files go, what it runs, which leftovers are ours, and the session
/// bookkeeping — kept apart from the launches so that the WSL one (第 1 段), the
/// SSH one (第 3 段) and the tests all work from the same definitions.
mod session {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use anyhow::{Context as _, Result, anyhow, ensure};
    use collections::HashMap;
    use gpui::{
        App, AsyncWindowContext, Context, Entity, Global, PromptButton, PromptLevel, SharedString,
        Task, WeakEntity,
    };
    use project::agent_sandbox::is_project_key;
    pub(super) use project::agent_sandbox::project_key;
    use project::{AgentWriteScope, AgentWriterKind, Project, SandboxedWriterGuard};
    use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
    use terminal::Terminal;
    use terminal_view::terminal_panel::TerminalPanel;
    use util::ResultExt as _;
    use workspace::{Toast, Workspace, notifications::NotificationId};

    use crate::agent_registry_ui::{add_consented_hosts, allowlist_write_error, missing_hosts};

    /// The hosts the CLI itself talks to: the model API (with `/api/oauth/*`)
    /// and the token endpoint of the paste-a-code login.
    pub(super) const CLAUDE_HOSTS: [&str; 2] = ["api.anthropic.com", "platform.claude.com"];

    /// Where each project's configuration directory lives in the distro
    /// account's home, below `sandbox::SANDBOXED_AGENT_CONFIG_HOME_RELATIVE`:
    /// hidden from, and in WSL not writable by, every other sandboxed command,
    /// since it holds login tokens and the hooks a session runs.
    ///
    /// Each project has its own directory in it, named by [`project_key`], so
    /// that settings, hooks, MCP servers and memory one project's session
    /// writes never reach another project's; each project signs in on its own.
    /// The session is granted its project's directory, and the helper shows it
    /// nothing else in this one, not even a project added while it runs. The
    /// helper protects `.claude`, `.mcp.json`, `.gemini` and `.codex` in every
    /// root a command may write, and a missing one keeps new entries out of the
    /// whole root; the CLI creates its files at the top of its config
    /// directory, so that directory is [`CONFIG_DIR_NAME`] one level down, an
    /// existing entry the grant covers whole. The bridge creates the lot,
    /// owner-only and without following links, when it claims the project's
    /// directory and writes the lock.
    const PROJECTS_HOME_RELATIVE: &str = sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE;
    const CONFIG_DIR_NAME: &str = "config";
    /// Where the CLI looks for IDE locks under `CLAUDE_CONFIG_DIR`.
    const LOCK_DIRECTORY_NAME: &str = "ide";
    /// The shape `sandbox::ide_rendezvous_path` documents:
    /// `<home>/.cache/forgekairos/ide/<session_id>/bridge.sock`.
    const RENDEZVOUS_HOME_RELATIVE: &str =
        branding::concat_const!(branding::CURRENT.remote_cache_home_relative, "/ide");
    const RENDEZVOUS_SOCKET_NAME: &str = "bridge.sock";
    /// Names this launcher's rendezvous directories, so the sweep leaves every
    /// other one alone.
    const SESSION_ID_PREFIX: &str = "claude-";
    /// Runs inside the sandbox: it sets what the helper keeps out of the
    /// environment it forwards (`USERPROFILE`) and what exists only there (the
    /// session's own `TMPDIR`), then becomes the CLI, whose absolute path is
    /// `$0` — never looked up on `PATH`, which reaches the Windows drives. In
    /// WSL the CLI asks `powershell.exe` for the Windows profile unless
    /// `USERPROFILE` is set; only its presence matters there.
    ///
    /// ADR-F56: with `ZED_IDE_ASSIST=1` it also reads the IDE session's token
    /// out of the lock file the bridge wrote (read-only inside the sandbox) into
    /// the variable the `--mcp-config` JSON names, as the whole `Authorization`
    /// value (`Bearer <token>`), so that the token never appears on a command
    /// line (ADR-F49 item 5). A lock that is not there or holds no token leaves
    /// the variable unset: the CLI then gets a 401 from the IDE tools rather
    /// than failing to start (checked with the real CLI 2.1.280).
    const LAUNCH_SCRIPT: &str = r#"CLAUDE_CODE_TMPDIR=${TMPDIR:-/tmp}
USERPROFILE=$CLAUDE_CONFIG_DIR
export CLAUDE_CODE_TMPDIR USERPROFILE
if [ "${ZED_IDE_ASSIST:-}" = 1 ] && [ -n "${CLAUDE_CODE_SSE_PORT:-}" ] && [ -r "$CLAUDE_CONFIG_DIR/ide/$CLAUDE_CODE_SSE_PORT.lock" ]; then
    IFS= read -r ZED_IDE_LOCK 2>/dev/null <"$CLAUDE_CONFIG_DIR/ide/$CLAUDE_CODE_SSE_PORT.lock" || :
    case ${ZED_IDE_LOCK:-} in
        *\"authToken\":\"*)
            ZED_IDE_BEARER=${ZED_IDE_LOCK#*\"authToken\":\"}
            ZED_IDE_BEARER=${ZED_IDE_BEARER%%\"*}
            ZED_IDE_BEARER="Bearer $ZED_IDE_BEARER"
            export ZED_IDE_BEARER
            ;;
    esac
    unset ZED_IDE_LOCK
fi
unset ZED_IDE_ASSIST
exec "$0" "$@""#;
    /// ADR-F56: tells [`LAUNCH_SCRIPT`] to read the IDE session's token.
    const ASSIST_FLAG_VARIABLE: &str = "ZED_IDE_ASSIST";
    const LAUNCH_SHELL: &str = "/bin/sh";

    pub(super) fn new_session_id() -> String {
        format!("{SESSION_ID_PREFIX}{}", uuid::Uuid::new_v4().simple())
    }

    /// Where a session's files live inside the distro.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct SessionLayout {
        /// Holds every session's rendezvous directory.
        pub(super) rendezvous_directory: String,
        /// The root the session may write.
        pub(super) config_root: String,
        /// `CLAUDE_CONFIG_DIR`.
        pub(super) config_dir: String,
        pub(super) lock_directory: String,
        pub(super) lock_file: String,
    }

    impl SessionLayout {
        pub(super) fn from_rendezvous(
            rendezvous: &str,
            session_id: &str,
            project_key: &str,
            inner_port: u16,
        ) -> Result<Self> {
            ensure!(
                is_project_key(project_key),
                i18n::t!("Invalid project identifier (internal error): {project_key:?}")
            );
            let unexpected = || {
                i18n::t!(
                    "Unexpected shape of the IDE rendezvous path (internal error): {rendezvous:?}"
                )
            };
            let rendezvous_directory = rendezvous
                .strip_suffix(&format!("/{session_id}/{RENDEZVOUS_SOCKET_NAME}"))
                .with_context(unexpected)?;
            let home = rendezvous_directory
                .strip_suffix(&format!("/{RENDEZVOUS_HOME_RELATIVE}"))
                .filter(|home| home.starts_with('/'))
                .with_context(unexpected)?;
            let (config_root, config_dir) = config_paths(home, project_key)?;
            let lock_directory = format!("{config_dir}/{LOCK_DIRECTORY_NAME}");
            let lock_file = format!("{lock_directory}/{inner_port}.lock");
            Ok(Self {
                rendezvous_directory: rendezvous_directory.to_string(),
                config_root,
                config_dir,
                lock_directory,
                lock_file,
            })
        }
    }

    /// ADR-F49 決定 15: the project's configuration root and, inside it, the
    /// directory the CLI is pointed at, below `home` — the account's home on
    /// whichever Linux the session runs on, so the WSL session and the SSH one
    /// place them by the same rule.
    pub(super) fn config_paths(home: &str, project_key: &str) -> Result<(String, String)> {
        ensure!(
            is_project_key(project_key),
            i18n::t!("Invalid project identifier (internal error): {project_key:?}")
        );
        ensure!(
            home.starts_with('/') && !home.ends_with('/') && !home.contains(['\n', '\0']),
            i18n::t!("Unexpected shape of the remote home directory (internal error): {home:?}")
        );
        let config_root = format!("{home}/{PROJECTS_HOME_RELATIVE}/{project_key}");
        let config_dir = format!("{config_root}/{CONFIG_DIR_NAME}");
        Ok((config_root, config_dir))
    }

    /// ADR-F53: where this session's IDE bridge listens on an SSH host — the
    /// same shape `sandbox::ide_rendezvous_path` builds inside a WSL distro, by
    /// the home the host itself reported. The bridge creates the directories on
    /// the way, owner-only and without following a link.
    pub(super) fn rendezvous_path(home: &str, session_id: &str) -> Result<String> {
        ensure!(
            is_session_id(session_id),
            i18n::t!("Invalid IDE session identifier (internal error): {session_id:?}")
        );
        ensure!(
            home.starts_with('/') && !home.ends_with('/') && !home.contains(['\n', '\0']),
            i18n::t!("Unexpected shape of the remote home directory (internal error): {home:?}")
        );
        Ok(format!(
            "{home}/{RENDEZVOUS_HOME_RELATIVE}/{session_id}/{RENDEZVOUS_SOCKET_NAME}"
        ))
    }

    /// What the CLI is started with. `ide_port` is the port inside the sandbox
    /// the IDE channel listens on, and `None` for a session without one (a
    /// sandbox whose IDE bridge could not be started): then nothing points the
    /// CLI at an IDE, so it runs as it would in a plain terminal instead of
    /// waiting for one.
    ///
    /// ADR-F56: `assist_tools` has [`LAUNCH_SCRIPT`] read the token the IDE
    /// tools' `--mcp-config` names; it only means anything with an IDE port.
    pub(super) fn session_environment(
        config_dir: &str,
        ide_port: Option<u16>,
        assist_tools: bool,
    ) -> Vec<(String, String)> {
        let mut environment = vec![("CLAUDE_CONFIG_DIR", config_dir.to_string())];
        if let Some(ide_port) = ide_port {
            environment.extend([
                // On its own this makes the CLI connect to the IDE whose lock is
                // named after this port.
                ("CLAUDE_CODE_SSE_PORT", ide_port.to_string()),
                // Otherwise the CLI works the address out with `ip route` in WSL.
                ("CLAUDE_CODE_IDE_HOST_OVERRIDE", "127.0.0.1".to_string()),
            ]);
            if assist_tools {
                environment.push((ASSIST_FLAG_VARIABLE, "1".to_string()));
            }
        }
        environment.extend([
            ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_string()),
            ("DISABLE_AUTOUPDATER", "1".to_string()),
            ("DISABLE_UPDATES", "1".to_string()),
            ("DISABLE_INSTALLATION_CHECKS", "1".to_string()),
            (
                "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
                "1".to_string(),
            ),
            ("ENABLE_CLAUDEAI_MCP_SERVERS", "false".to_string()),
            ("CLAUDE_CODE_DISABLE_ARTIFACT", "1".to_string()),
            // Neither `wsl.exe` nor the helper carries the terminal's own over.
            ("TERM", "xterm-256color".to_string()),
            ("COLORTERM", "truecolor".to_string()),
        ]);
        environment
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    /// ADR-F49 決定 1: how the CLI is started inside the sandbox — the shell
    /// that sets what only exists there and then becomes the CLI, with
    /// `claude_path` as `$0`. Shared by both sandboxes; the caller appends
    /// nothing after `effort`.
    ///
    /// ADR-F56: `assist_port` is the IDE channel's port inside the sandbox when
    /// the CLI is to get Zed's IDE tools: the `zed` server on the same channel's
    /// `/mcp/zed`, every assist tool allowed up front. Its token is left for the
    /// CLI to expand from the environment ([`LAUNCH_SCRIPT`]).
    pub(super) fn launch_invocation(
        claude_path: &str,
        effort: Option<&str>,
        assist_port: Option<u16>,
    ) -> Result<(String, Vec<String>)> {
        ensure!(
            claude_path.starts_with('/') && !claude_path.contains(['\n', '\0']),
            i18n::t!(
                "The location of the Claude Code used in the sandbox is not an absolute path (internal error): {claude_path:?}"
            )
        );
        let mut args = vec![
            "-c".to_string(),
            LAUNCH_SCRIPT.to_string(),
            claude_path.to_string(),
        ];
        if let Some(port) = assist_port {
            args.extend(ide_server::cli_assist_arguments(&format!(
                "http://127.0.0.1:{port}{}",
                ide_server::ASSIST_MCP_PATH
            )));
        }
        if let Some(effort) = effort {
            args.extend(["--effort".to_string(), effort.to_string()]);
        }
        Ok((LAUNCH_SHELL.to_string(), args))
    }

    /// Everything the sandbox is asked for, before any of it is resolved.
    #[cfg(any(target_os = "windows", test))]
    #[derive(Debug)]
    pub(super) struct SessionPlan {
        pub(super) command: sandbox::CommandAndArgs,
        /// The worktree roots (host paths), then the config root (in WSL).
        pub(super) writable_paths: Vec<PathBuf>,
        pub(super) allowed_hosts: Vec<String>,
        pub(super) ide_forward: sandbox::IdeForward,
    }

    #[cfg(any(target_os = "windows", test))]
    pub(super) fn plan_session(
        layout: &SessionLayout,
        rendezvous: &str,
        claude_path: &str,
        effort: Option<&str>,
        roots: &[PathBuf],
        inner_port: u16,
        assist_tools: bool,
    ) -> Result<SessionPlan> {
        let first_root = roots
            .first()
            .context(i18n::t!("There is no folder to run Claude Code in."))?;
        let (program, args) =
            launch_invocation(claude_path, effort, assist_tools.then_some(inner_port))?;
        let mut writable_paths = roots.to_vec();
        writable_paths.push(PathBuf::from(&layout.config_root));
        Ok(SessionPlan {
            command: sandbox::CommandAndArgs {
                program,
                args,
                env: session_environment(&layout.config_dir, Some(inner_port), assist_tools)
                    .into_iter()
                    .collect(),
                cwd: Some(first_root.clone()),
            },
            writable_paths,
            allowed_hosts: CLAUDE_HOSTS.iter().map(|host| host.to_string()).collect(),
            ide_forward: sandbox::IdeForward {
                inner_port,
                rendezvous_path: rendezvous.to_string(),
            },
        })
    }

    /// The hosts consent's detail, which says the hosts go into the IDE-wide
    /// allowlist, where the built-in agent's commands can reach them too.
    pub(super) fn hosts_detail(missing: &[SharedString]) -> String {
        let hosts = CLAUDE_HOSTS
            .iter()
            .map(|host| {
                if missing.iter().any(|missing| missing.as_ref() == *host) {
                    i18n::t!("{host} (not added)")
                } else {
                    i18n::t!("{host} (added)")
                }
            })
            .collect::<Vec<_>>()
            .join(i18n::tc!("list separator", ", "));
        i18n::t!(
            "In the sandbox, Claude Code connects only to the following Anthropic hosts. They are added to the IDE-wide allowlist and also apply to the built-in agent's commands.\nHosts that need to be allowed: {hosts}"
        )
    }

    /// The projects whose sandboxed session is still starting, by
    /// [`session_key`]: from the launch until its terminal exists. Once it
    /// does, the next launch starts another session on the same
    /// configuration, as several CLIs sharing one `~/.claude` do.
    #[derive(Clone, Default)]
    pub(super) struct StartingSessions(Rc<RefCell<StartingSessionsState>>);

    #[derive(Default)]
    struct StartingSessionsState {
        by_key: HashMap<String, u64>,
        next_generation: u64,
    }

    impl Global for StartingSessions {}

    impl StartingSessions {
        /// `None` while another launch for `key` is still starting.
        pub(super) fn begin(&self, key: String) -> Option<StartingSession> {
            let mut state = self.0.borrow_mut();
            if state.by_key.contains_key(&key) {
                return None;
            }
            let generation = state.next_generation;
            state.next_generation += 1;
            state.by_key.insert(key.clone(), generation);
            Some(StartingSession {
                sessions: self.clone(),
                key,
                generation,
            })
        }
    }

    /// A launch's hold on its key while it starts. Given back once the
    /// session's terminal exists ([`Self::started`]), or when dropped.
    pub(super) struct StartingSession {
        sessions: StartingSessions,
        key: String,
        generation: u64,
    }

    impl StartingSession {
        pub(super) fn started(&mut self) {
            self.release();
        }

        /// Only while the key is still this launch's: once released, a later
        /// launch may have taken it, and removing that one's would let a
        /// third launch start alongside it.
        fn release(&self) {
            let mut state = self.sessions.0.borrow_mut();
            if state.by_key.get(&self.key) == Some(&self.generation) {
                state.by_key.remove(&self.key);
            }
        }
    }

    impl Drop for StartingSession {
        fn drop(&mut self) {
            self.release();
        }
    }

    /// One configuration: where it runs — the distro by its registered name
    /// (WSL compares names without case), or the host by its connection — and
    /// the project's key.
    pub(super) fn session_key(where_it_runs: Option<&str>, project_key: &str) -> String {
        format!(
            "{}/{project_key}",
            where_it_runs.unwrap_or_default().to_lowercase()
        )
    }

    /// ADR-F50: what a session can write, spelled as worktrees are: its
    /// folders, and — through the distro's share, where the WSL home can be a
    /// worktree too — its configuration root, when `distro` names the distro.
    /// Shared with the external agents started in the WSL sandbox (第 2b 段).
    #[cfg(any(target_os = "windows", test))]
    pub(super) fn session_write_scope(
        roots: &[PathBuf],
        config_root: &str,
        distro: Option<&str>,
    ) -> Vec<PathBuf> {
        project::agent_sandbox::wsl_session_write_scope(roots, config_root, distro)
    }

    /// ADR-F50 on an SSH host: the roots and the configuration root are the
    /// host's own paths, which is how its worktrees are spelled too, so there
    /// is no second spelling to add — unlike WSL, where the same folder is
    /// also reachable through `\\wsl.localhost\<distro>\…`.
    pub(super) fn remote_session_write_scope(roots: &[PathBuf], config_root: &str) -> Vec<PathBuf> {
        let mut scope = roots.to_vec();
        scope.push(PathBuf::from(config_root));
        scope
    }

    /// ADR-F50: registers the session as the writer of `scope` until the
    /// returned guard drops: each worktree in it, in any open project, is
    /// paused at once, and resuming one waits for the session's end. The CLI
    /// starts once the returned task is done, which pauses the worktrees
    /// reached through other spellings of `scope` as well.
    pub(super) fn register_session_writer(
        project: &Entity<Project>,
        scope: Vec<PathBuf>,
        cx: &mut App,
    ) -> (SandboxedWriterGuard, Task<()>) {
        project.update(cx, |project, cx| {
            // A command, not an agent turn: nothing in the agent panel stops
            // this CLI, so a resume has to wait for its session to end.
            project.register_agent_writer(
                AgentWriteScope::Paths(scope),
                AgentWriterKind::Command,
                cx,
            )
        })
    }

    #[cfg(any(target_os = "windows", test))]
    const SWEEP_DIRECTORY_PREFIX: &str = "zed-sweep-dir ";
    #[cfg(any(target_os = "windows", test))]
    const SWEEP_LOCK_PREFIX: &str = "zed-sweep-lock ";
    #[cfg(any(target_os = "windows", test))]
    const SWEEP_COMMAND_PREFIX: &str = "zed-sweep-cmd ";

    /// Leftovers of sessions whose bridge is gone (Zed or WSL went down
    /// before it could clean up).
    #[cfg(any(target_os = "windows", test))]
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct StaleEntries {
        pub(super) rendezvous: Vec<String>,
        pub(super) locks: Vec<String>,
    }

    #[cfg(any(target_os = "windows", test))]
    impl StaleEntries {
        pub(super) fn is_empty(&self) -> bool {
            self.rendezvous.is_empty() && self.locks.is_empty()
        }
    }

    /// From the sweep's listing (see `launch::SWEEP_LIST_SCRIPT`): the entries
    /// named the way this launcher names them that no running process — a
    /// bridge, or the helper connecting to it — names on its command line.
    #[cfg(any(target_os = "windows", test))]
    pub(super) fn stale_entries(listing: &str, layout: &SessionLayout) -> StaleEntries {
        let mut directories = Vec::new();
        let mut locks = Vec::new();
        let mut running = Vec::new();
        for line in listing.lines() {
            if let Some(name) = line.strip_prefix(SWEEP_DIRECTORY_PREFIX) {
                directories.push(name);
            } else if let Some(name) = line.strip_prefix(SWEEP_LOCK_PREFIX) {
                locks.push(name);
            } else if let Some(arguments) = line.strip_prefix(SWEEP_COMMAND_PREFIX) {
                running.push(format!("{} ", arguments.trim_end()));
            }
        }
        let in_use = |needle: String| running.iter().any(|arguments| arguments.contains(&needle));
        StaleEntries {
            rendezvous: directories
                .into_iter()
                .filter(|name| is_session_id(name))
                .filter(|name| !in_use(format!("{}/{name}/", layout.rendezvous_directory)))
                .map(str::to_string)
                .collect(),
            locks: locks
                .into_iter()
                .filter(|name| is_lock_file_name(name))
                .filter(|name| !in_use(format!("{}/{name} ", layout.lock_directory)))
                .map(str::to_string)
                .collect(),
        }
    }

    fn is_session_id(name: &str) -> bool {
        name.strip_prefix(SESSION_ID_PREFIX).is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    }

    #[cfg(any(target_os = "windows", test))]
    fn is_lock_file_name(name: &str) -> bool {
        name.strip_suffix(".lock").is_some_and(|port| {
            (1..=5).contains(&port.len())
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        })
    }

    #[cfg(any(target_os = "windows", test))]
    const HELPER_FAILURE_MARKER: &str = "zed-wsl-sandbox: ";

    /// Why the session never started, when the helper refused to build the
    /// sandbox: it exits 126 after a marker line, which lands on the session's
    /// terminal screen rather than in anything Zed reads.
    #[cfg(any(target_os = "windows", test))]
    pub(super) fn helper_failure_message(exit_code: Option<u32>, screen: &str) -> Option<String> {
        let start = std::iter::once(0)
            .chain(screen.match_indices('\n').map(|(index, _)| index + 1))
            .find(|&start| {
                screen
                    .get(start..)
                    .is_some_and(|rest| rest.starts_with(HELPER_FAILURE_MARKER))
            })?;
        acp_thread::wsl_sandbox_setup_failure_message(exit_code, screen.get(start..)?)
    }

    pub(super) const LAUNCH_FAILED_PREFIX: &str =
        i18n::msg!("Could not start Claude Code (sandbox): ");
    pub(super) const PREPARING_MESSAGE: &str = i18n::msg!("Preparing Claude Code (sandbox)…");
    pub(super) const PREPARING_SANDBOX_MESSAGE: &str =
        i18n::msg!("Preparing the sandbox and IDE integration…");
    pub(super) const SANDBOX_TAB_LABEL: &str = i18n::msg!("Claude Code (Sandbox)");
    const HOSTS_MESSAGE: &str =
        i18n::msg!("Claude Code (sandbox) needs hosts added to the allowlist to connect");
    const ADD_HOSTS_LABEL: &str = i18n::msg!("Add to Allowlist and Start");
    const DECLINE_HOSTS_LABEL: &str = i18n::msg!("Cancel Without Adding");

    struct SandboxLaunchProgress;

    pub(super) fn progress_id() -> NotificationId {
        NotificationId::unique::<SandboxLaunchProgress>()
    }

    pub(super) fn show_progress(
        workspace: &mut Workspace,
        message: &str,
        cx: &mut Context<Workspace>,
    ) {
        workspace.show_toast(Toast::new(progress_id(), message.to_string()), cx);
    }

    pub(super) const STILL_STARTING_MESSAGE: &str = i18n::msg!(
        "Claude Code (sandbox) is still starting for this project. Try again once it has started."
    );

    struct StillStarting;

    pub(super) fn still_starting_id() -> NotificationId {
        NotificationId::unique::<StillStarting>()
    }

    /// A launch [`begin_session`] turned away. The progress notice on screen
    /// belongs to the earlier launch that still starts, so the refused one
    /// must not dismiss it.
    #[derive(Debug)]
    pub(super) struct LaunchStillStarting;

    impl std::fmt::Display for LaunchStillStarting {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(i18n::tr(STILL_STARTING_MESSAGE))
        }
    }

    impl std::error::Error for LaunchStillStarting {}

    /// The launch's hold on `key` while it starts, or `None` after saying that
    /// an earlier launch for it has not finished starting.
    pub(super) fn begin_session(
        key: String,
        workspace: &WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Option<StartingSession> {
        let starting = cx.default_global::<StartingSessions>().clone();
        let session = starting.begin(key);
        if session.is_none() {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_toast(
                        Toast::new(
                            still_starting_id(),
                            i18n::tr(STILL_STARTING_MESSAGE).to_string(),
                        )
                        .autohide(),
                        cx,
                    );
                })
                .log_err();
        }
        session
    }

    pub(super) enum HostsConsent {
        Declined,
        /// `network_ceiling` is the editor-wide allowlist the session's
        /// network nests inside; `None` when this process has none.
        Granted {
            network_ceiling: Option<Vec<String>>,
        },
    }

    /// The editor-wide allowlist caps what the session may reach, so the
    /// Anthropic hosts it lacks are added — with the user's consent, as for an
    /// agent install (ADR-F12) — before the session starts.
    pub(super) async fn consent_to_claude_hosts(
        cx: &mut AsyncWindowContext,
    ) -> Result<HostsConsent> {
        let hosts = CLAUDE_HOSTS
            .iter()
            .map(|host| SharedString::new_static(host))
            .collect::<Vec<_>>();
        let (ceiling, missing) =
            cx.update(|_, cx| (editor_allowlist(cx), missing_hosts(&hosts, cx)))?;
        let Some(mut ceiling) = ceiling else {
            return Ok(HostsConsent::Granted {
                network_ceiling: None,
            });
        };
        if missing.is_empty() {
            return Ok(HostsConsent::Granted {
                network_ceiling: Some(ceiling),
            });
        }
        let detail = hosts_detail(&missing);
        let answer = cx.prompt(
            PromptLevel::Warning,
            i18n::tr(HOSTS_MESSAGE),
            Some(&detail),
            &[
                PromptButton::ok(i18n::tr(ADD_HOSTS_LABEL)),
                PromptButton::cancel(i18n::tr(DECLINE_HOSTS_LABEL)),
            ],
        );
        if !answer.await.is_ok_and(|index| index == 0) {
            return Ok(HostsConsent::Declined);
        }
        let added = missing.iter().map(ToString::to_string).collect::<Vec<_>>();
        cx.update(|_, cx| add_consented_hosts(added.clone(), cx))?
            .await
            .map_err(|error| anyhow!(allowlist_write_error(&error)))?;
        // The allowlist's watcher takes the file in a little later; the
        // session starts now, with the hosts the user just agreed to.
        ceiling.extend(added);
        Ok(HostsConsent::Granted {
            network_ceiling: Some(ceiling),
        })
    }

    /// The editor-wide allowlist as it stands now, as the sandbox helper's
    /// proxy takes it; `None` in a process that has none.
    pub(super) fn editor_allowlist(cx: &App) -> Option<Vec<String>> {
        let global = cx.try_global::<egress_policy::EgressAllowlistGlobal>()?;
        Some(
            global
                .0
                .snapshot()
                .patterns()
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    }

    /// Names a session's terminal task after its session id, which is also the
    /// key its IDE session is started under ([`PendingIdeSession::start`]).
    const SANDBOX_TASK_ID_PREFIX: &str = "terminal::claude-code-sandbox::";

    /// ADR-F56: the IDE session key of the sandboxed CLI running in `terminal`,
    /// or `None` for any other terminal.
    pub(crate) fn cli_tab_session_key(terminal: &Terminal) -> Option<String> {
        terminal
            .task()?
            .spawned_task
            .id
            .0
            .strip_prefix(SANDBOX_TASK_ID_PREFIX)
            .map(str::to_string)
    }

    /// Opens the session's terminal tab on `(program, args, environment)` —
    /// the local `wsl.exe` invocation of the WSL sandbox, or the `ssh` one the
    /// project's transport builds for the host's helper. Shared so that both
    /// sandboxes land in the same place: the agent panel's tab (ADR-F53), or
    /// the terminal panel when there is no panel to put it in.
    pub(super) async fn spawn_terminal(
        workspace: &WeakEntity<Workspace>,
        program: String,
        args: Vec<String>,
        environment: Vec<(String, String)>,
        session_id: &str,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<Terminal>> {
        let spawn_task = SpawnInTerminal {
            id: TaskId(format!("{SANDBOX_TASK_ID_PREFIX}{session_id}")),
            full_label: i18n::tr(SANDBOX_TAB_LABEL).to_string(),
            label: i18n::tr(SANDBOX_TAB_LABEL).to_string(),
            command: Some(program),
            args,
            command_label: i18n::tr(SANDBOX_TAB_LABEL).to_string(),
            env: environment.into_iter().collect(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            show_summary: false,
            show_command: false,
            // The same line run again would find the session it belongs to
            // gone: its bridge, or its registration as the folders' writer.
            show_rerun: false,
            ..SpawnInTerminal::default()
        };
        // ADR-F53: 置き場はエージェントパネルのタブ。パネルが無いときだけ
        // 今までどおりターミナルパネルに開く。
        if let Some(panel) = workspace.read_with(cx, |workspace, cx| {
            super::agent_panel_for_cli_tab(workspace, cx)
        })? {
            return super::open_terminal_tab_in_agent_panel(
                workspace,
                panel,
                spawn_task,
                super::SessionTerminalKind::Sandboxed,
                cx,
            )
            .await;
        }

        let terminal_panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))?
            .context(i18n::t!("The terminal panel is not available."))?;
        let terminal = terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_with(
                    RevealStrategy::Always,
                    window,
                    cx,
                    move |project, cx| project.create_sandboxed_terminal_task(spawn_task, cx),
                )
            })?
            .await?;
        terminal.upgrade().context(i18n::t!(
            "The session's terminal was closed right after it opened."
        ))
    }

    /// The IDE channel, which both sandboxes have: WSL's (ADR-F49 決定 3) and an
    /// SSH host's (ADR-F53). What differs is only where the bridge process is
    /// started and which path mapper the session reads paths with — the session
    /// itself, the lock file and the multiplexing are the same.
    pub(super) use ide_link::{IdeLink, PendingIdeSession};

    mod ide_link {
        use std::path::PathBuf;
        use std::sync::Arc;

        use futures::channel::mpsc;
        use futures::{AsyncRead, AsyncWrite, StreamExt as _};
        use gpui::{App, Task, WeakEntity};
        use workspace::Workspace;

        /// The IDE session, started before the bridge: its auth token goes into
        /// the lock file the bridge writes before any connection can arrive.
        pub(crate) struct PendingIdeSession {
            session: ide_server::SandboxIdeSessionHandle,
            connections: mpsc::UnboundedSender<sandbox::BridgeConnection>,
        }

        /// Keeps the IDE side of a session alive. Dropping it ends the bridge's
        /// stdin, so the bridge removes its socket and lock and exits, and ends
        /// the IDE session, which closes its diff tabs.
        pub(crate) struct IdeLink {
            _mux: sandbox::BridgeMuxHandle,
            _forward: Task<()>,
            _session: ide_server::SandboxIdeSessionHandle,
        }

        impl PendingIdeSession {
            /// `session_id` names the session's terminal task too
            /// (`spawn_terminal`), which is how the agent panel finds the IDE
            /// session of a CLI tab (ADR-F56, `AddSelectionToThread`).
            pub(crate) fn start(
                workspace: WeakEntity<Workspace>,
                mapper: Arc<dyn ide_server::PathMapper>,
                roots: Vec<PathBuf>,
                session_id: &str,
                cx: &mut App,
            ) -> (Self, String) {
                let (sender, receiver) = mpsc::unbounded();
                let (session, info) = ide_server::start_sandbox_session_for_tab(
                    workspace,
                    receiver,
                    mapper,
                    roots,
                    session_id.to_string(),
                    cx,
                );
                (
                    Self {
                        session,
                        connections: sender,
                    },
                    info.auth_token,
                )
            }

            /// Takes the bridge's multiplexed stdout (`reader`) and stdin
            /// (`writer`) apart into the session's connections.
            pub(crate) fn attach<R, W>(self, reader: R, writer: W, cx: &mut App) -> IdeLink
            where
                R: AsyncRead + Unpin + Send + 'static,
                W: AsyncWrite + Unpin + Send + 'static,
            {
                let executor = cx.background_executor().clone();
                let (mut connections, mux) = sandbox::spawn_bridge_mux(reader, writer, |future| {
                    executor.spawn(future).detach()
                });
                let sender = self.connections;
                let forward = executor.spawn(async move {
                    while let Some(connection) = connections.next().await {
                        if sender.unbounded_send(connection).is_err() {
                            break;
                        }
                    }
                });
                IdeLink {
                    _mux: mux,
                    _forward: forward,
                    _session: self.session,
                }
            }
        }
    }
}

/// ADR-F49 第 3 段: the sandboxed session on the Linux host of an SSH-remote
/// project.
///
/// The helper is the host's own `remote_server`, re-invoked as the sandbox
/// helper (ADR-F18) — already uploaded and verified by the connection, so no
/// second binary and no proto change. The CLI is fetched **on this machine**,
/// where the release key and the allowlist are, and sent to the host, so a host
/// with no way out to the network still runs a checked build.
///
/// ADR-F53: it has the same IDE channel the WSL sandbox has. The bridge is the
/// host's helper once more, started over the project's own transport with piped
/// standard I/O (`start_remote_ide_bridge`); the requests the session makes
/// about files are checked **on the host** — `Project::verify_agent_read`
/// (proto 506), which the remote server answers by opening the path from the
/// worktree root through no link at all — because the paths are the host's and
/// this machine cannot open them.
///
/// A bridge that will not start is said out loud and the CLI runs without an
/// IDE, exactly as it did before ADR-F53; it is never quietly counted as
/// success.
mod remote_launch {
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{Context as _, Result, anyhow, bail};
    use collections::HashMap;
    use futures::AsyncWriteExt as _;
    use gpui::{AsyncWindowContext, Context, Entity, Task, WeakEntity, Window};
    use project::{Project, SandboxedWriterGuard};
    use remote::{Interactive, RemoteClient};
    use terminal::Terminal;
    use util::ResultExt as _;
    use util::path_mapping::ComparablePath;
    use util::paths::PathStyle;
    use workspace::Workspace;

    use super::session::{
        CLAUDE_HOSTS, HostsConsent, IdeLink, LAUNCH_FAILED_PREFIX, LaunchStillStarting,
        PREPARING_MESSAGE, PREPARING_SANDBOX_MESSAGE, PendingIdeSession, SessionLayout,
        begin_session, config_paths, consent_to_claude_hosts, launch_invocation, new_session_id,
        progress_id, project_key, register_session_writer, remote_session_write_scope,
        rendezvous_path, session_environment, session_key, show_progress, spawn_terminal,
    };

    /// Where the staged CLI and anything else this launcher leaves on the host
    /// lives (see `managed_claude_binary`), relative to the account's home.
    /// Protected for every session, so that one whose folder *is* the account's
    /// home cannot overwrite the very binary it runs: in WSL the helper adds
    /// this itself, but on an SSH host it is the client that has to ask.
    const HOST_CACHE_HOME_RELATIVE: &str = branding::CURRENT.remote_cache_home_relative;
    /// [`HOST_CACHE_HOME_RELATIVE`] as builds from before the rename named it:
    /// a build from before the rename still stages and runs its CLI there.
    const LEGACY_HOST_CACHE_HOME_RELATIVE: &str =
        branding::ZED_JP_ENHANCE.remote_cache_home_relative;

    /// The cache directories every session on the host at `home` is kept out
    /// of, under the current name and the earlier one.
    pub(super) fn host_cache_protected_paths(home: &str) -> [PathBuf; 2] {
        [HOST_CACHE_HOME_RELATIVE, LEGACY_HOST_CACHE_HOME_RELATIVE]
            .map(|relative| PathBuf::from(format!("{home}/{relative}")))
    }

    /// Makes the project's configuration root (`$1`) and the directory the CLI
    /// is pointed at inside it (`$2`), owner-only: it holds the session's login
    /// token and the hooks it runs. `umask` covers the parents `mkdir -p`
    /// creates on the way, and the two known directories are set explicitly in
    /// case they are older than this.
    const CREATE_CONFIG_SCRIPT: &str = r#"set -u
umask 077
mkdir -p "$2" || exit 1
chmod 700 "$1" "$2" 2>/dev/null || true
"#;

    pub(super) fn launch_on_remote_host(
        workspace: &mut Workspace,
        effort: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let (project, roots, key_roots, git_dirs, remote_client) = {
            let project = workspace.project().read(cx);
            (
                workspace.project().clone(),
                super::sandbox_roots(project, cx),
                project::agent_sandbox::project_key_roots(project.worktree_store().read(cx), cx),
                acp_thread::sandbox_git_dirs(project, cx),
                project.remote_client(),
            )
        };
        let Some(remote_client) = remote_client else {
            super::show_launch_failure(
                workspace,
                format!(
                    "{}{}",
                    i18n::tr(LAUNCH_FAILED_PREFIX),
                    i18n::t!("The project is not connected to the remote host.")
                ),
                effort,
                cx,
            );
            return;
        };
        show_progress(workspace, i18n::tr(PREPARING_MESSAGE), cx);
        cx.spawn_in(window, async move |workspace, cx| {
            let outcome = start_session(
                workspace.clone(),
                project,
                remote_client,
                roots,
                key_roots,
                git_dirs,
                effort.clone(),
                cx,
            )
            .await;
            if outcome
                .as_ref()
                .is_err_and(|error| error.is::<LaunchStillStarting>())
            {
                return;
            }
            workspace
                .update(cx, |workspace, cx| {
                    workspace.dismiss_toast(&progress_id(), cx);
                    if let Err(error) = outcome {
                        super::show_launch_failure(
                            workspace,
                            format!("{}{error:#}", i18n::tr(LAUNCH_FAILED_PREFIX)),
                            effort,
                            cx,
                        );
                    }
                })
                .log_err();
        })
        .detach();
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "everything the session is built from, read from the workspace before it starts"
    )]
    async fn start_session(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        remote_client: Entity<RemoteClient>,
        roots: Vec<PathBuf>,
        key_roots: Vec<PathBuf>,
        git_dirs: Vec<PathBuf>,
        effort: Option<String>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        if roots.is_empty() {
            bail!(i18n::t!("There is no folder to run Claude Code in."));
        }
        // The key is the project's folders, not the session's writable ones,
        // and is left in the host's own spelling: resolving links is the host's
        // to do and this machine cannot, while the worktrees are the only
        // spelling it is ever given — so the same project keys the same way
        // every time, which is what the key is for.
        let project_key = project_key(&key_roots);
        let (connection, remote_binary) = remote_client.read_with(cx, |remote_client, _| {
            (
                remote_client.connection_options().display_name(),
                remote_client.remote_server_binary_path(),
            )
        });
        let remote_binary = remote_binary.context(i18n::t!(
            "The Zed server on the remote host is not ready yet. Wait for the connection to finish and try again."
        ))?;
        // Held only while this launch starts: a session that runs does not
        // stop another from starting on the same configuration.
        let key = session_key(Some(&connection), &project_key);
        let Some(mut starting) = cx.update(|_, cx| begin_session(key, &workspace, cx))? else {
            return Err(LaunchStillStarting.into());
        };
        let HostsConsent::Granted { network_ceiling } = consent_to_claude_hosts(cx).await? else {
            return Ok(());
        };
        // As in the WSL launch: the installer's own failures carry an English
        // detail behind the Japanese headline, and `{error:#}` at the call site
        // would splice it into the toast, so collapse to the headline here.
        let binary = claude_binary(workspace.clone(), remote_client.clone(), cx)
            .await
            .map_err(|error| {
                log::warn!("[claude-sandbox] could not prepare Claude Code: {error:#}");
                anyhow!("{error}")
            })?;
        let home = binary.home_on_host.context(i18n::t!(
            "Could not determine the home directory on the remote host (internal error)."
        ))?;
        let (config_root, config_dir) = config_paths(&home, &project_key)?;
        workspace.update(cx, |workspace, cx| {
            show_progress(workspace, i18n::tr(PREPARING_SANDBOX_MESSAGE), cx)
        })?;
        // The helper skips a write grant that is not there yet (it logs and
        // carries on), so the CLI would start unable to write its own
        // configuration — and unable to keep a sign-in. ADR-F53's bridge would
        // create them too, but a session whose bridge did not start has to be
        // able to write its configuration all the same.
        crate::managed_claude_binary::run_remote_host_script(
            remote_client.clone(),
            CREATE_CONFIG_SCRIPT,
            vec![config_root.clone(), config_dir.clone()],
            cx,
        )
        .await
        .context(i18n::t!(
            "Could not create the Claude Code settings folder on the remote host."
        ))?;

        let session_id = new_session_id();
        // ADR-F53: the IDE channel. Everything of it that can fail does so
        // before the CLI starts, and a failure leaves the session running
        // without an IDE rather than not running at all.
        let ide = match start_remote_ide_bridge(
            &workspace,
            &remote_client,
            &home,
            &session_id,
            &project_key,
            &roots,
            cx,
        )
        .await
        {
            Ok(ide) => Some(ide),
            Err(error) => {
                log::warn!("[claude-sandbox] the IDE bridge did not start: {error:#}");
                workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .show_error(format!("{}{error}", i18n::tr(IDE_UNAVAILABLE_PREFIX)), cx)
                    })
                    .log_err();
                None
            }
        };
        let ide_port = ide.as_ref().map(|ide| ide.forward.inner_port);
        // ADR-F56: the IDE tools ride on the same channel, so only with one.
        let assist_tools = cx.update(|_, cx| ide_server::assist_tools_enabled(cx))?;
        let assist_port = ide_port.filter(|_| assist_tools);
        let (program, args) =
            launch_invocation(&binary.path_on_host, effort.as_deref(), assist_port)?;
        let environment = session_environment(&config_dir, ide_port, assist_port.is_some());
        let first_root = roots
            .first()
            .context(i18n::t!("There is no folder to run Claude Code in."))?
            .clone();
        let wrap = acp_thread::SandboxWrap {
            writable_paths: roots
                .iter()
                .cloned()
                .chain([PathBuf::from(&config_root)])
                .collect(),
            extra_write_paths: Vec::new(),
            network: acp_thread::SandboxNetworkAccess::Restricted(
                http_proxy::Allowlist::from_patterns(
                    CLAUDE_HOSTS
                        .iter()
                        .map(|host| http_proxy::HostPattern::parse(host))
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(|error| {
                            anyhow!(i18n::t!(
                                "Invalid host for Claude Code to connect to (internal error): {error}"
                            ))
                        })?,
                ),
            ),
            // The `.git` paths arrive spelled the way the host spells them
            // (`acp_thread::sandbox_git_dirs` uses the project's path style),
            // and the cache is built from the home the host reported.
            protected_paths: git_dirs
                .into_iter()
                .chain(host_cache_protected_paths(&home))
                .collect(),
            allow_fs_write: false,
            // The proxy that would confine it runs on the host, inside the
            // helper, not in this process.
            is_local: false,
        };
        // ADR-F46: the helper's own process is given no environment; everything
        // the CLI gets travels in the helper's own argv through `--setenv`, and
        // the helper applies it inside its jail. The transport turns this into
        // the `ssh` line when the terminal starts it, which is also what gives
        // it a pty — so nothing is built here but the helper's invocation.
        let (helper_program, helper_args) = acp_thread::build_remote_sandbox_command(
            &remote_binary,
            &wrap,
            network_ceiling.as_deref(),
            Some(&first_root),
            &environment.into_iter().collect::<HashMap<_, _>>(),
            &[],
            true,
            ide.as_ref().map(|ide| &ide.forward),
            Some(&session_id),
            program,
            args,
        );

        // ADR-F50: the CLI writes its folders and its configuration from its
        // first moment, so it is their writer before it starts.
        let write_scope = remote_session_write_scope(&roots, &config_root);
        let (worktree_writer, worktree_writer_resolved) =
            cx.update(|_, cx| register_session_writer(&project, write_scope, cx))?;
        // From here on, dropping `resources` (an error below included) ends the
        // bridge, the IDE session and the hold on the worktrees.
        let resources = SessionResources {
            ide,
            worktree_writer,
        };
        worktree_writer_resolved.await;
        let terminal = spawn_terminal(
            &workspace,
            helper_program,
            helper_args,
            Vec::new(),
            &session_id,
            cx,
        )
        .await?;
        starting.started();
        watch_session(terminal, resources, workspace, cx)
    }

    /// The CLI the session runs: the managed Linux build, fetched and verified
    /// here and staged on the host (ADR-F49 decision 2 / 第 3 段).
    async fn claude_binary(
        workspace: WeakEntity<Workspace>,
        remote_client: Entity<RemoteClient>,
        cx: &mut AsyncWindowContext,
    ) -> Result<crate::ManagedClaudeBinary> {
        cx.update(|window, cx| {
            crate::ensure_managed_claude_binary(
                crate::ManagedClaudeTarget::RemoteHost(remote_client),
                window,
                cx,
                move |progress, _window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_progress(workspace, &progress.message(), cx)
                        })
                        .log_err();
                },
            )
        })?
        .await
    }

    /// ADR-F53: start this session's IDE channel on the SSH host.
    ///
    /// The order is fixed, because each step needs the one before it:
    ///
    /// 1. the IDE session is started here, so that its auth token can go into
    ///    the lock file before any connection can arrive
    /// 2. the host's helper is started once more as the bridge, over the
    ///    project's own transport, with piped standard I/O and the lock content
    ///    as a stdin preamble (never on a command line the host's `/proc` shows)
    /// 3. it is waited for, so that its rendezvous socket exists before the
    ///    sandbox helper — which refuses a rendezvous that is not there — looks
    ///    for it
    ///
    /// `Interactive::No`: the bridge speaks the frame protocol on its standard
    /// I/O and must not be given a pty. The helper is named by its
    /// `$HOME`-relative path, which the transport resolves, exactly as the
    /// sandbox helper itself is started.
    ///
    /// The paths the session reads are the host's own, so the mapper is the
    /// identity one and every check on them happens on the host (`ide_server`'s
    /// remote `SandboxChecks`).
    ///
    /// The concrete standard-I/O types differ between platforms
    /// ([`util::command`]), so everything that touches them stays in this one
    /// function.
    async fn start_remote_ide_bridge(
        workspace: &WeakEntity<Workspace>,
        remote_client: &Entity<RemoteClient>,
        home: &str,
        session_id: &str,
        project_key: &str,
        roots: &[PathBuf],
        cx: &mut AsyncWindowContext,
    ) -> Result<RemoteIdeChannel> {
        const PREAMBLE_FAILED: &str =
            i18n::msg!("Could not pass the connection details to the IDE integration relay");

        let inner_port = sandbox::new_inner_ide_port();
        let rendezvous = rendezvous_path(home, session_id)?;
        // The helper refuses a rendezvous the command itself may write — the
        // socket it connects to from outside every namespace could be replaced
        // — and it refuses the whole command over it, which would leave the CLI
        // unable to start at all. A project whose folder *is* the account's
        // home is exactly that case, so the channel is left out with a reason.
        if let Some(root) = writable_root_holding(roots, &rendezvous) {
            bail!(i18n::t!(
                "The relay cannot be started because the project folder ({root}) contains the IDE integration's rendezvous location."
            ));
        }

        // Taken before the process is started, and given back by every path out
        // of here that does not return the channel.
        let Some(bridge_slot) = sandbox::IdeBridgeSlot::take() else {
            log::warn!(
                "refusing an IDE bridge: {} are already running",
                sandbox::MAX_IDE_BRIDGES
            );
            bail!(i18n::t!(
                "{count} IDE integration relays are already running, so no more can be started. Close the agent threads and terminals you are not using and try again.",
                count = sandbox::MAX_IDE_BRIDGES
            ));
        };

        let layout =
            SessionLayout::from_rendezvous(&rendezvous, session_id, project_key, inner_port)?;
        let mapper: Arc<dyn ide_server::PathMapper> = Arc::new(ide_server::IdentityPathMapper);
        let (pending, auth_token) = cx.update(|_, cx| {
            PendingIdeSession::start(
                workspace.clone(),
                mapper.clone(),
                roots.to_vec(),
                session_id,
                cx,
            )
        })?;
        let workspace_folders = roots
            .iter()
            .filter_map(|root| ide_server::PathMapper::to_client(mapper.as_ref(), root))
            .collect::<Vec<_>>();
        let preamble = sandbox::encode_ide_lock_preamble(&ide_server::sandbox_lock_file_contents(
            &auth_token,
            &workspace_folders,
        ));
        let template = remote_client.read_with(cx, |remote_client, _| {
            let helper = remote_client.remote_server_binary_path().context(i18n::t!(
                "The Zed server on the remote host is not ready yet (internal error)."
            ))?;
            // No exclusive directory: sessions on the same configuration run
            // side by side, each with its own port and lock file.
            let arguments =
                sandbox::remote_ide_bridge_argv(&rendezvous, Some(&layout.lock_file), None)?;
            remote_client.build_command(
                Some(helper.display(PathStyle::Unix).into_owned()),
                &arguments,
                &HashMap::default(),
                None,
                None,
                Interactive::No,
            )
        })?;

        let mut command = util::command::new_command(&template.program);
        command
            .args(&template.args)
            .envs(template.env)
            .stdin(util::command::Stdio::piped())
            .stdout(util::command::Stdio::piped())
            .stderr(util::command::Stdio::piped())
            .kill_on_drop(true);
        let mut bridge = command.spawn().context(i18n::t!(
            "Could not start the IDE integration relay on the remote host"
        ))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) = (
            bridge.stdin.take(),
            bridge.stdout.take(),
            bridge.stderr.take(),
        ) else {
            bail!("bug: the IDE bridge's standard I/O is not piped");
        };
        stdin
            .write_all(&preamble)
            .await
            .context(i18n::tr(PREAMBLE_FAILED))?;
        stdin.flush().await.context(i18n::tr(PREAMBLE_FAILED))?;

        let executor = cx.background_executor().clone();
        let bridge_log = sandbox::wait_for_ide_bridge_ready(
            stderr,
            sandbox::BridgeHost::RemoteHost,
            executor.timer(sandbox::IDE_BRIDGE_READY_TIMEOUT),
        )
        .await?;
        let bridge_log = executor.spawn(bridge_log);
        let link = cx.update(|_, cx| pending.attach(stdout, stdin, cx))?;
        Ok(RemoteIdeChannel {
            forward: sandbox::IdeForward {
                inner_port,
                rendezvous_path: rendezvous,
            },
            _link: link,
            _bridge: bridge,
            _bridge_log: bridge_log,
            _bridge_slot: bridge_slot,
        })
    }

    /// The folder the session may write that holds `rendezvous`, if any.
    ///
    /// Both are the host's own spelling (absolute Unix paths), so they are read
    /// by the host's rules rather than with [`Path::starts_with`], which on a
    /// Windows client would read them by the wrong ones — and by components
    /// rather than as text, so that a folder spelled `/srv//app` or `/srv/app/`
    /// is still seen to hold what is under `/srv/app`. A root the host reaches
    /// through a link is beyond what this machine can tell; the helper refuses
    /// the rendezvous then, which is a refusal and not a way in.
    pub(super) fn writable_root_holding(roots: &[PathBuf], rendezvous: &str) -> Option<String> {
        let rendezvous = ComparablePath::new(Path::new(rendezvous), PathStyle::Unix)?;
        roots.iter().find_map(|root| {
            let comparable = ComparablePath::new(root, PathStyle::Unix)?;
            (!comparable.components().is_empty() && rendezvous.starts_with(&comparable))
                .then(|| root.to_string_lossy().into_owned())
        })
    }

    /// ADR-F53: the prefix of the toast shown when the session starts but its
    /// IDE channel could not, so that the CLI's own diff tabs and diagnostics
    /// are missing for a reason the user can read.
    const IDE_UNAVAILABLE_PREFIX: &str = i18n::msg!(
        "Claude Code will start, but the IDE integration (diff tabs and diagnostics) is not available: "
    );

    /// ADR-F53: the IDE channel of a session on an SSH host, and everything that
    /// keeps it alive. Dropping it ends the bridge's stdin, so the bridge
    /// removes its socket and its lock file on the host and exits.
    struct RemoteIdeChannel {
        /// What the helper is told to open inside the command's own network
        /// namespace, and where to splice it to.
        forward: sandbox::IdeForward,
        /// Dropped first (fields drop in declaration order): it closes the IDE
        /// session and the multiplexer.
        _link: IdeLink,
        /// Killed when dropped, which closes the bridge's stdin on the host, so
        /// it removes its socket and its lock file and exits.
        _bridge: util::command::Child,
        _bridge_log: Task<()>,
        /// This bridge's place in [`sandbox::MAX_IDE_BRIDGES`], given back last,
        /// once the `ssh` that holds it is gone.
        _bridge_slot: sandbox::IdeBridgeSlot,
    }

    /// What a running remote session holds on to. There is no sandbox object on
    /// this machine: the helper on the host owns both its namespaces and its
    /// proxy, and the terminal's own process is the `ssh` that runs it.
    struct SessionResources {
        /// ADR-F53: `None` for a session whose bridge would not start.
        ide: Option<RemoteIdeChannel>,
        /// ADR-F50: the session's hold on the worktrees it can write.
        worktree_writer: SandboxedWriterGuard,
    }

    /// Frees the session's hold on the worktrees when the CLI exits or its
    /// terminal closes, and says why when the host's helper refused to build
    /// the sandbox (ADR-F18's fail-closed marker).
    fn watch_session(
        terminal: Entity<Terminal>,
        resources: SessionResources,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let completion = cx.update(|_, cx| terminal.read(cx).wait_for_completed_task(cx))?;
        let terminal = terminal.downgrade();
        cx.spawn(async move |cx| {
            let status = completion.await;
            let SessionResources {
                ide,
                worktree_writer,
            } = resources;
            // Ends the bridge's stdin: it removes its socket and its lock file
            // on the host and exits. Dropping the `ssh` stops it either way.
            drop(ide);
            // The CLI has exited, so nothing of its writes is still to come.
            drop(worktree_writer);
            let exit_code = status
                .and_then(|status| status.code())
                .and_then(|code| u32::try_from(code).ok());
            let message = terminal
                .read_with(cx, |terminal, _| {
                    acp_thread::remote_sandbox_setup_failure_display_message(
                        exit_code,
                        &terminal.get_content(),
                    )
                })
                .ok()
                .flatten();
            if let Some(message) = message
                && let Some(workspace) = workspace.upgrade()
            {
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx));
            }
        })
        .detach();
        Ok(())
    }
}

/// The sandboxed launch itself, which needs WSL.
#[cfg(target_os = "windows")]
mod launch {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context as _, Result, anyhow, bail};
    use fs::Fs;
    use gpui::{
        AsyncWindowContext, BackgroundExecutor, Context, Entity, FutureExt as _, Task, WeakEntity,
        Window,
    };
    use project::SandboxedWriterGuard;
    use project::agent_sandbox::canonical_project_key_roots;
    use terminal::Terminal;
    use util::ResultExt as _;
    use util::command::Stdio;
    use workspace::Workspace;

    use super::session::{
        HostsConsent, IdeLink, LAUNCH_FAILED_PREFIX, LaunchStillStarting, PREPARING_MESSAGE,
        PREPARING_SANDBOX_MESSAGE, PendingIdeSession, SessionLayout, SessionPlan, begin_session,
        consent_to_claude_hosts, helper_failure_message, new_session_id, plan_session, progress_id,
        project_key, register_session_writer, session_key, session_write_scope, show_progress,
        spawn_terminal, stale_entries,
    };

    /// One `wsl.exe` round trip, which a cold WSL can take this long to answer.
    const WSL_STEP_TIMEOUT: Duration = Duration::from_secs(90);
    const BRIDGE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Lists, for [`stale_entries`], the entries of the rendezvous directory
    /// (`$1`) and the lock directory (`$2`) and the command lines that name an
    /// IDE rendezvous. A directory reached through a symlink is not listed.
    const SWEEP_LIST_SCRIPT: &str = r#"set -u
list() {
    cd -P -- "$2" 2>/dev/null && [ "$(pwd -P)" = "$2" ] || return 0
    for entry in *; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        case "$entry" in *"
"*) continue ;; esac
        printf 'zed-sweep-%s %s\n' "$1" "$entry"
    done
}
(list dir "$1")
(list lock "$2")
for cmdline in /proc/[0-9]*/cmdline; do
    arguments=$(tr '\0\n' '  ' < "$cmdline" 2>/dev/null) || continue
    case "$arguments" in
        *--ide-bridge*|*--ide-rendezvous*) printf 'zed-sweep-cmd %s\n' "$arguments" ;;
    esac
done
"#;

    /// Removes what [`stale_entries`] chose: `$1` and `$2` as for the list,
    /// `$3` how many of the names that follow are rendezvous directories (the
    /// rest are lock files). Each directory is entered first and checked to be
    /// the one named, so that one replaced by a symlink since is left alone
    /// (the lock directory is inside what the session may write).
    const SWEEP_REMOVE_SCRIPT: &str = r#"set -u
directory=$1
locks=$2
count=$3
shift 3
(
    cd -P -- "$directory" 2>/dev/null && [ "$(pwd -P)" = "$directory" ] || exit 0
    index=0
    for name in "$@"; do
        index=$((index + 1))
        [ "$index" -le "$count" ] || break
        case "$name" in */*) continue ;; claude-*) rm -rf -- "./$name" ;; esac
    done
)
(
    cd -P -- "$locks" 2>/dev/null && [ "$(pwd -P)" = "$locks" ] || exit 0
    index=0
    for name in "$@"; do
        index=$((index + 1))
        [ "$index" -gt "$count" ] || continue
        case "$name" in */*|.*) continue ;; *.lock) rm -f -- "./$name" ;; esac
    done
)
"#;

    pub(super) fn launch_in_sandbox(
        workspace: &mut Workspace,
        effort: Option<String>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let (roots, key_roots, git_dirs) = {
            let project = workspace.project().read(cx);
            (
                super::sandbox_roots(project, cx),
                // ADR-F49 decision 15: the folders the configuration is named
                // after are the same everywhere, so that the CLI here and an
                // ACP agent in its sandbox share one folder and one sign-in.
                project::agent_sandbox::project_key_roots(project.worktree_store().read(cx), cx),
                acp_thread::sandbox_git_dirs(project, cx),
            )
        };
        let distro = match distro_for_roots(&roots) {
            Ok(distro) => distro,
            Err(error) => {
                super::show_launch_failure(
                    workspace,
                    format!("{}{error:#}", i18n::tr(LAUNCH_FAILED_PREFIX)),
                    effort,
                    cx,
                );
                return;
            }
        };
        let fs = workspace.app_state().fs.clone();
        show_progress(workspace, i18n::tr(PREPARING_MESSAGE), cx);
        cx.spawn_in(window, async move |workspace, cx| {
            let outcome = start_session(
                workspace.clone(),
                fs,
                roots,
                key_roots,
                git_dirs,
                distro,
                effort.clone(),
                cx,
            )
            .await;
            if outcome
                .as_ref()
                .is_err_and(|error| error.is::<LaunchStillStarting>())
            {
                return;
            }
            workspace
                .update(cx, |workspace, cx| {
                    workspace.dismiss_toast(&progress_id(), cx);
                    if let Err(error) = outcome {
                        super::show_launch_failure(
                            workspace,
                            format!("{}{error:#}", i18n::tr(LAUNCH_FAILED_PREFIX)),
                            effort,
                            cx,
                        );
                    }
                })
                .log_err();
        })
        .detach();
    }

    /// The distro the session runs in: the one a `\\wsl.localhost\<distro>\`
    /// root names, else the default one.
    fn distro_for_roots(roots: &[PathBuf]) -> Result<Option<String>> {
        project::agent_sandbox::wsl_distro_for_roots(roots)
    }

    async fn start_session(
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        roots: Vec<PathBuf>,
        key_roots: Vec<PathBuf>,
        git_dirs: Vec<PathBuf>,
        distro: Option<String>,
        effort: Option<String>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        if roots.is_empty() {
            bail!(i18n::t!("There is no folder to run Claude Code in."));
        }
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
        // The key's folders are the project's, not the session's writable
        // ones: a single file's folder names the project without the session
        // being allowed to write it.
        let canonical_roots = canonical_project_key_roots(&fs, key_roots).await;
        let project_key = project_key(&canonical_roots);
        // The default distro by its name, so that it and a path naming it share
        // one key, as they share one configuration. Without the registry's
        // word, the name the project's paths give, if any.
        let distro_name = sandbox::registered_wsl_distro_name(distro.as_deref()).or_else(|| {
            log::warn!("[claude-sandbox] the registry does not name WSL distro {distro:?}");
            distro.clone()
        });
        let key = session_key(distro_name.as_deref(), &project_key);
        // Held only while this launch starts: a session that runs does not
        // stop another from starting on the same configuration.
        let Some(mut starting) = cx.update(|_, cx| begin_session(key, &workspace, cx))? else {
            return Err(LaunchStillStarting.into());
        };
        let HostsConsent::Granted { network_ceiling } = consent_to_claude_hosts(cx).await? else {
            return Ok(());
        };
        // `claude_path`'s errors are mostly `managed_claude_binary::refuse`'s,
        // whose English detail sits behind the error's alternate `Display` on
        // purpose. The caller of `start_session` prints its failure with
        // `{error:#}`, which would walk that chain and splice the English
        // detail into the Japanese toast, so collapse to the single Japanese
        // headline here first (ADR-F49 決定 43 と同じ方針: 訳せない詳細は本文に
        // 混ぜない)。`refuse` logs its own detail, but the installer's other
        // failures (a Japanese sentence over an English I/O error) do not, and
        // this is the last place that detail exists.
        let claude_path = claude_path(workspace.clone(), distro.clone(), cx)
            .await
            .map_err(|error| {
                log::warn!("[claude-sandbox] could not prepare Claude Code: {error:#}");
                anyhow!("{error}")
            })?;
        workspace.update(cx, |workspace, cx| {
            show_progress(workspace, i18n::tr(PREPARING_SANDBOX_MESSAGE), cx)
        })?;

        let executor = cx.background_executor().clone();
        let session_id = new_session_id();
        let inner_port = sandbox::new_inner_ide_port();
        let rendezvous = in_wsl(&executor, {
            let distro = distro.clone();
            let session_id = session_id.clone();
            async move { sandbox::ide_rendezvous_path(distro.as_deref(), &session_id).await }
        })
        .await?;
        let layout =
            SessionLayout::from_rendezvous(&rendezvous, &session_id, &project_key, inner_port)?;
        let assist_tools = cx.update(|_, cx| ide_server::assist_tools_enabled(cx))?;
        let plan = plan_session(
            &layout,
            &rendezvous,
            &claude_path,
            effort.as_deref(),
            &roots,
            inner_port,
            assist_tools,
        )?;

        let mapper: Arc<dyn ide_server::PathMapper> = Arc::new(ide_server::WslPathMapper {
            // The default distro's name is not needed: its paths reach the IDE
            // session only as `/mnt/<drive>/…`, which the mapper takes alone.
            distro: distro.clone().unwrap_or_default(),
        });
        let (pending, auth_token) = cx.update(|_, cx| {
            PendingIdeSession::start(
                workspace.clone(),
                mapper.clone(),
                roots.clone(),
                &session_id,
                cx,
            )
        })?;
        let workspace_folders = roots
            .iter()
            .filter_map(|root| ide_server::PathMapper::to_client(mapper.as_ref(), root))
            .collect::<Vec<_>>();
        let lock_content = ide_server::sandbox_lock_file_contents(&auth_token, &workspace_folders);
        let (wsl_program, bridge_args, preamble) = in_wsl(&executor, {
            let distro = distro.clone();
            let rendezvous = rendezvous.clone();
            let lock_file = layout.lock_file.clone();
            // No exclusive directory: sessions on the same configuration run
            // side by side, each with its own port and lock file.
            async move {
                sandbox::ide_bridge_invocation(
                    distro.as_deref(),
                    &rendezvous,
                    Some((&lock_file, &lock_content)),
                    None,
                )
                .await
            }
        })
        .await?;
        if let Err(error) = in_wsl(
            &executor,
            sweep_stale_sessions(wsl_program.clone(), distro.clone(), layout.clone()),
        )
        .await
        {
            log::warn!("[claude-sandbox] could not remove what earlier sessions left: {error:#}");
        }

        let sandbox::IdeBridge {
            child: bridge,
            stdin: bridge_stdin,
            stdout: bridge_stdout,
            log: bridge_log,
            slot: bridge_slot,
        } = sandbox::start_ide_bridge(
            &wsl_program,
            &bridge_args,
            &preamble,
            executor.timer(sandbox::IDE_BRIDGE_READY_TIMEOUT),
        )
        .await?;
        let bridge_log = executor.spawn(bridge_log);
        let ide = cx.update(|_, cx| pending.attach(bridge_stdout, bridge_stdin, cx))?;
        // ADR-F50: the CLI writes its folders and its configuration from its
        // first moment to the session's end, so it is their writer before it
        // starts and for as long as `resources` lives.
        let write_scope = session_write_scope(&roots, &layout.config_root, distro_name.as_deref());
        let (worktree_writer, worktree_writer_resolved) =
            cx.update(|_, cx| register_session_writer(&project, write_scope, cx))?;
        // From here on, dropping `resources` (an error below included) ends the
        // bridge, the IDE session and the hold on the worktrees.
        let mut resources = SessionResources {
            ide,
            bridge,
            _bridge_log: bridge_log,
            sandbox: None,
            worktree_writer,
            bridge_slot,
        };
        worktree_writer_resolved.await;
        let (wrapped, sandbox) =
            in_wsl(&executor, wrap_session(plan, git_dirs, network_ceiling)).await?;
        resources.sandbox = Some(sandbox);
        let terminal = spawn_terminal(
            &workspace,
            wrapped.program,
            wrapped.args,
            wrapped.env.into_iter().collect(),
            &session_id,
            cx,
        )
        .await?;
        starting.started();
        watch_session(terminal, resources, workspace, cx)
    }

    /// Development only: the absolute path, inside the distro, of a program to
    /// run in place of Claude Code, for checking the session and its IDE
    /// channel end to end without signing in to anything.
    #[cfg(debug_assertions)]
    const STAND_IN_VARIABLE: &str = "ZED_SANDBOXED_CLAUDE_STAND_IN";

    /// The CLI the session runs: the managed Linux build, which asks before
    /// the first download of each version (ADR-F49 decision 2).
    async fn claude_path(
        workspace: WeakEntity<Workspace>,
        distro: Option<String>,
        cx: &mut AsyncWindowContext,
    ) -> Result<String> {
        #[cfg(debug_assertions)]
        if let Some(stand_in) = std::env::var(STAND_IN_VARIABLE)
            .ok()
            .filter(|path| path.starts_with('/'))
        {
            log::warn!("[claude-sandbox] running {stand_in} in place of Claude Code");
            return Ok(stand_in);
        }
        let binary = cx
            .update(|window, cx| {
                crate::ensure_managed_claude_binary(
                    crate::ManagedClaudeTarget::WslDistro(distro),
                    window,
                    cx,
                    move |progress, _window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                show_progress(workspace, &progress.message(), cx)
                            })
                            .log_err();
                    },
                )
            })?
            .await?;
        Ok(binary.path_on_host)
    }

    async fn in_wsl<T: Send + 'static>(
        executor: &BackgroundExecutor,
        work: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        executor
            .spawn(work)
            .with_timeout(WSL_STEP_TIMEOUT, executor)
            .await
            .map_err(|_| {
                anyhow!(i18n::t!(
                    "WSL did not respond within {seconds} seconds. Wait a moment and try again; if that does not help, restart WSL with `wsl --shutdown`.",
                    seconds = WSL_STEP_TIMEOUT.as_secs()
                ))
            })?
    }

    async fn sweep_stale_sessions(
        wsl_program: PathBuf,
        distro: Option<String>,
        layout: SessionLayout,
    ) -> Result<()> {
        let directories = [
            layout.rendezvous_directory.clone(),
            layout.lock_directory.clone(),
        ];
        let listing = run_wsl_script(
            &wsl_program,
            distro.as_deref(),
            SWEEP_LIST_SCRIPT,
            &directories,
        )
        .await?;
        let stale = stale_entries(&listing, &layout);
        if stale.is_empty() {
            return Ok(());
        }
        log::info!("[claude-sandbox] removing what earlier sessions left: {stale:?}");
        let mut arguments = directories.to_vec();
        arguments.push(stale.rendezvous.len().to_string());
        arguments.extend(stale.rendezvous);
        arguments.extend(stale.locks);
        run_wsl_script(
            &wsl_program,
            distro.as_deref(),
            SWEEP_REMOVE_SCRIPT,
            &arguments,
        )
        .await?;
        Ok(())
    }

    async fn run_wsl_script(
        wsl_program: &Path,
        distro: Option<&str>,
        script: &str,
        arguments: &[String],
    ) -> Result<String> {
        let mut command = util::command::new_command(wsl_program);
        if let Some(distro) = distro {
            command.args(["-d", distro]);
        }
        command
            .args(["--exec", "/bin/sh", "-c", script, "zed-claude-sweep"])
            .args(arguments)
            .env("WSLENV", "")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = command.output().await.context("running wsl.exe")?;
        anyhow::ensure!(
            output.status.success(),
            "wsl.exe exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn wrap_session(
        plan: SessionPlan,
        git_dirs: Vec<PathBuf>,
        network_ceiling: Option<Vec<String>>,
    ) -> Result<(sandbox::WrappedCommand, sandbox::Sandbox)> {
        let mut writable_paths = Vec::with_capacity(plan.writable_paths.len());
        for path in &plan.writable_paths {
            writable_paths.push(sandbox::HostFilesystemLocation::capture(path).with_context(
                || {
                    i18n::t!(
                        "Could not check the folder `{path}` that writes are allowed to",
                        path = path.display()
                    )
                },
            )?);
        }
        // As for the agent's commands: a `.git` that cannot be captured is not
        // there to protect.
        let protected_paths = git_dirs
            .iter()
            .filter_map(|path| sandbox::HostFilesystemLocation::capture(path).ok())
            .collect();
        let mut sandbox = sandbox::Sandbox::new(sandbox::SandboxPolicy {
            fs: sandbox::SandboxFsPolicy::Restricted {
                writable_paths: sandbox::normalize_host_filesystem_locations(
                    writable_paths.into_iter(),
                ),
                protected_paths,
                readable_paths: Vec::new(),
            },
            network: sandbox::SandboxNetPolicy::Restricted {
                allowed_domains: plan.allowed_hosts,
            },
        })?;
        if let Some(entries) = network_ceiling {
            sandbox.set_editor_network_allowlist(entries);
        }
        sandbox.set_interactive_session(sandbox::InteractiveSession {
            ide_forward: Some(plan.ide_forward),
        });
        let wrapped = sandbox.wrap(&plan.command).await?;
        Ok((wrapped, sandbox))
    }

    /// What a running session holds on to.
    struct SessionResources {
        ide: IdeLink,
        bridge: util::command::Child,
        _bridge_log: Task<()>,
        sandbox: Option<sandbox::Sandbox>,
        /// ADR-F50: the session's hold on the worktrees it can write.
        worktree_writer: SandboxedWriterGuard,
        /// ADR-F49 (new decision): this bridge's place in
        /// [`sandbox::MAX_IDE_BRIDGES`], given back once its `wsl.exe` is gone
        /// so that the count never runs ahead of the bridges that exist.
        bridge_slot: sandbox::IdeBridgeSlot,
    }

    impl SessionResources {
        async fn shut_down(self, executor: &BackgroundExecutor) {
            let SessionResources {
                ide,
                mut bridge,
                _bridge_log,
                sandbox,
                worktree_writer,
                bridge_slot,
            } = self;
            // Ends the bridge's stdin: it removes its socket and lock and exits.
            drop(ide);
            drop(sandbox);
            // The CLI has exited, so nothing of its writes is still to come.
            drop(worktree_writer);
            match bridge
                .status()
                .with_timeout(BRIDGE_EXIT_TIMEOUT, executor)
                .await
            {
                Ok(Ok(status)) => log::debug!("[claude-sandbox] the IDE bridge exited: {status}"),
                Ok(Err(error)) => {
                    log::warn!("[claude-sandbox] waiting for the IDE bridge: {error}")
                }
                // Dropping it stops `wsl.exe`; the next launch sweeps what is left.
                Err(_) => log::warn!("[claude-sandbox] the IDE bridge did not exit; stopping it"),
            }
            // The bridge first: one that ran past the timeout is still
            // there until this drop stops it, and handing its place on to the
            // next one first would let more `wsl.exe` run than the limit says.
            drop(bridge);
            drop(bridge_slot);
        }
    }

    /// Ends the bridge and the IDE session when the CLI exits or its terminal
    /// closes, and says why when the helper refused to start it.
    fn watch_session(
        terminal: Entity<Terminal>,
        resources: SessionResources,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Result<()> {
        let completion = cx.update(|_, cx| terminal.read(cx).wait_for_completed_task(cx))?;
        let terminal = terminal.downgrade();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |cx| {
            let status = completion.await;
            resources.shut_down(&executor).await;
            let exit_code = status
                .and_then(|status| status.code())
                .and_then(|code| u32::try_from(code).ok());
            let message = terminal
                .read_with(cx, |terminal, _| {
                    helper_failure_message(exit_code, &terminal.get_content())
                })
                .ok()
                .flatten();
            if let Some(message) = message
                && let Some(workspace) = workspace.upgrade()
            {
                workspace.update(cx, |workspace, cx| workspace.show_error(message, cx));
            }
        })
        .detach();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;

    use async_tungstenite::WebSocketStream;
    use async_tungstenite::tungstenite::Message;
    use futures::channel::mpsc;
    use futures::{AsyncRead, AsyncReadExt as _, AsyncWrite, StreamExt as _};
    use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
    use project::worktree_code_execution::{ResumeOutcome, WorktreeCodeState};
    use project::{FakeFs, Project, WorktreeCodeExecution, WorktreeId};
    use serde_json::{Value, json};
    use util::path;
    use workspace::{MultiWorkspace, Workspace};
    use zed_actions::terminal::{LaunchClaudeCode, LaunchClaudeCodeOutsideSandbox};

    use super::session::{
        CLAUDE_HOSTS, PendingIdeSession, SessionLayout, StaleEntries, StartingSessions,
        config_paths, helper_failure_message, hosts_detail, launch_invocation, plan_session,
        project_key, register_session_writer, remote_session_write_scope, rendezvous_path,
        session_environment, session_key, session_write_scope, stale_entries,
    };
    use super::{
        AiPlacement, CLAUDE_PROGRAM, COLLABORATION_REASON, HostLauncher, LaunchRoute,
        NO_FOLDER_REASON, OPEN_SANDBOX_SETTINGS_LABEL, OUTSIDE_SANDBOX_CLAUDE_LOGIN,
        OUTSIDE_SANDBOX_LIMITS_LOCAL, OUTSIDE_SANDBOX_LIMITS_REMOTE, OUTSIDE_SANDBOX_MESSAGE,
        OUTSIDE_SANDBOX_NEW_SESSION, OUTSIDE_SANDBOX_NOT_ALLOWED_DETAIL,
        OUTSIDE_SANDBOX_NOT_ALLOWED_TITLE, OUTSIDE_SANDBOX_NOTICE, OUTSIDE_SANDBOX_ONCE,
        OUTSIDE_SANDBOX_OTHER_LOGIN, OutsideSandboxStart, ProjectShape, REMOTE_CONNECTION_REASON,
        REMOTE_NOT_LINUX_REASON, RemoteShape, SandboxLaunchFailure, UNSUPPORTED_OS_REASON,
        outside_sandbox_detail, outside_sandbox_message, placement_of_route,
        register_launch_actions, route_for, show_launch_failure,
    };

    const SESSION_ID: &str = "claude-0123456789abcdef0123456789abcdef";
    const RENDEZVOUS: &str = branding::concat_const!(
        "/root/",
        branding::CURRENT.remote_cache_home_relative,
        "/ide/claude-0123456789abcdef0123456789abcdef/bridge.sock"
    );
    const PORT: u16 = 45123;
    const CLAUDE_PATH: &str = branding::concat_const!(
        "/root/",
        branding::CURRENT.remote_cache_home_relative,
        "/agents/3a62/claude"
    );
    const PROJECT_KEY: &str = "0123456789abcdef";
    const CONFIG_ROOT: &str = branding::concat_const!(
        "/root/",
        sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE,
        "/0123456789abcdef"
    );
    const CACHE: &str = branding::CURRENT.remote_cache_home_relative;

    fn layout() -> SessionLayout {
        SessionLayout::from_rendezvous(RENDEZVOUS, SESSION_ID, PROJECT_KEY, PORT)
            .expect("the shape `sandbox::ide_rendezvous_path` documents")
    }

    #[test]
    fn launches_in_the_sandbox_only_where_it_can() {
        let sandboxable = ProjectShape {
            is_collaborative: false,
            remote: None,
            has_folder: true,
            sandbox_platform: true,
            outside_by_user_choice: false,
        };
        let ssh_linux = RemoteShape {
            is_ssh: true,
            is_linux: true,
        };
        assert_eq!(route_for(sandboxable), LaunchRoute::LocalSandbox);
        // 第 3 段: an SSH host runs the session in its own helper, whatever this
        // machine is — the sandbox is the host's.
        for sandbox_platform in [true, false] {
            assert_eq!(
                route_for(ProjectShape {
                    remote: Some(ssh_linux),
                    sandbox_platform,
                    ..sandboxable
                }),
                LaunchRoute::RemoteSandbox
            );
        }
        // ADR-F49 決定 19: the user's own choice to start it outside every
        // time wins over every shape, the sandboxable ones included.
        for shape in [
            sandboxable,
            ProjectShape {
                remote: Some(ssh_linux),
                ..sandboxable
            },
            ProjectShape {
                is_collaborative: true,
                has_folder: false,
                ..sandboxable
            },
        ] {
            assert_eq!(
                route_for(ProjectShape {
                    outside_by_user_choice: true,
                    ..shape
                }),
                LaunchRoute::OutsideSandboxByUserChoice,
                "{shape:?}"
            );
        }
        for (shape, reason) in [
            (
                ProjectShape {
                    remote: Some(RemoteShape {
                        is_ssh: false,
                        is_linux: true,
                    }),
                    ..sandboxable
                },
                REMOTE_CONNECTION_REASON,
            ),
            (
                ProjectShape {
                    remote: Some(RemoteShape {
                        is_ssh: true,
                        is_linux: false,
                    }),
                    ..sandboxable
                },
                REMOTE_NOT_LINUX_REASON,
            ),
            (
                ProjectShape {
                    is_collaborative: true,
                    ..sandboxable
                },
                COLLABORATION_REASON,
            ),
            // Collaboration wins over the connection: the session's folders are
            // ones other people see.
            (
                ProjectShape {
                    is_collaborative: true,
                    remote: Some(ssh_linux),
                    ..sandboxable
                },
                COLLABORATION_REASON,
            ),
            (
                ProjectShape {
                    sandbox_platform: false,
                    ..sandboxable
                },
                UNSUPPORTED_OS_REASON,
            ),
            (
                ProjectShape {
                    has_folder: false,
                    ..sandboxable
                },
                NO_FOLDER_REASON,
            ),
            // A remote project with no folder has nothing to run in either.
            (
                ProjectShape {
                    remote: Some(ssh_linux),
                    has_folder: false,
                    ..sandboxable
                },
                NO_FOLDER_REASON,
            ),
        ] {
            assert_eq!(
                route_for(shape),
                LaunchRoute::OutsideSandbox { reason },
                "{shape:?}"
            );
        }
    }

    /// ADR-F94: the "Choose AI" menu says where the CLI will run from the
    /// route the launch itself takes, so the badge cannot claim a sandbox the
    /// launch then skips.
    #[test]
    fn the_menu_badge_follows_the_launch_route() {
        assert_eq!(
            placement_of_route(LaunchRoute::RemoteSandbox),
            AiPlacement::Sandbox
        );
        assert_eq!(
            placement_of_route(LaunchRoute::OutsideSandbox {
                reason: NO_FOLDER_REASON
            }),
            AiPlacement::OutsideAfterConfirmation
        );
        assert_eq!(
            placement_of_route(LaunchRoute::OutsideSandboxByUserChoice),
            AiPlacement::OutsideByChoice
        );
        assert_eq!(
            placement_of_route(LaunchRoute::LocalSandbox),
            if cfg!(target_os = "windows") {
                AiPlacement::Sandbox
            } else {
                AiPlacement::OutsideAfterConfirmation
            }
        );
    }

    #[test]
    fn the_session_files_follow_the_rendezvous() {
        assert_eq!(
            layout(),
            SessionLayout {
                rendezvous_directory: format!("/root/{CACHE}/ide"),
                config_root: CONFIG_ROOT.to_string(),
                config_dir: format!("{CONFIG_ROOT}/config"),
                lock_directory: format!("{CONFIG_ROOT}/config/ide"),
                lock_file: format!("{CONFIG_ROOT}/config/ide/45123.lock"),
            }
        );
        for unexpected in [
            format!("/root/{CACHE}/ide/claude-other/bridge.sock"),
            format!("/root/elsewhere/{SESSION_ID}/bridge.sock"),
            format!("/{CACHE}/ide/{SESSION_ID}/bridge.sock"),
            // Where builds from before the rename put it.
            format!("/root/.cache/zed-jp-enhance/ide/{SESSION_ID}/bridge.sock"),
        ] {
            assert!(
                SessionLayout::from_rendezvous(&unexpected, SESSION_ID, PROJECT_KEY, PORT).is_err(),
                "{unexpected}"
            );
        }
        for key in ["../other", "0123", "0123456789ABCDEF"] {
            assert!(
                SessionLayout::from_rendezvous(RENDEZVOUS, SESSION_ID, key, PORT).is_err(),
                "{key}"
            );
        }
    }

    /// ADR-F53: the SSH host's rendezvous is placed by the same rule the WSL one
    /// is, so the layout built from it names the same configuration directory
    /// `config_paths` does, and a home or a session id of the wrong shape is
    /// refused rather than pasted into a path.
    #[test]
    fn the_remote_rendezvous_sits_where_the_wsl_one_does() {
        let rendezvous = rendezvous_path("/root", SESSION_ID).expect("a plain Linux home");
        assert_eq!(rendezvous, RENDEZVOUS);
        let from_home = SessionLayout::from_rendezvous(&rendezvous, SESSION_ID, PROJECT_KEY, PORT)
            .expect("the shape the launcher builds");
        assert_eq!(from_home, layout());
        let (config_root, config_dir) =
            config_paths("/root", PROJECT_KEY).expect("a plain Linux home");
        assert_eq!(
            (from_home.config_root, from_home.config_dir),
            (config_root, config_dir),
            "the SSH session signs in where every other session of this project does"
        );

        for home in ["", "root", "/root/", "/root\nrm -rf /", "/root\0"] {
            assert!(rendezvous_path(home, SESSION_ID).is_err(), "{home:?}");
        }
        for session_id in [
            "",
            "claude-0123",
            "../escape",
            "claude-0123456789ABCDEF0123456789abcdef",
            "other-0123456789abcdef0123456789abcdef",
        ] {
            assert!(
                rendezvous_path("/root", session_id).is_err(),
                "{session_id}"
            );
        }

        // 空白・引用符・日本語を含むホーム: パスは組み立てられ、そこから読み戻した配置も
        // `config_paths` と一致する (接続先へ渡すときの引用は SSH のトランスポートの仕事)。
        for home in [
            "/home/my user",
            "/home/o'brien",
            "/home/$USER",
            "/home/ユーザー",
        ] {
            let rendezvous = rendezvous_path(home, SESSION_ID)
                .unwrap_or_else(|error| panic!("{home:?}: {error:#}"));
            assert_eq!(
                rendezvous,
                format!("{home}/{CACHE}/ide/{SESSION_ID}/bridge.sock")
            );
            let layout = SessionLayout::from_rendezvous(&rendezvous, SESSION_ID, PROJECT_KEY, PORT)
                .unwrap_or_else(|error| panic!("{home:?}: {error:#}"));
            let (config_root, config_dir) = config_paths(home, PROJECT_KEY)
                .unwrap_or_else(|error| panic!("{home:?}: {error:#}"));
            assert_eq!(
                (layout.config_root, layout.config_dir),
                (config_root, config_dir)
            );
        }
    }

    /// ADR-F53: a project folder that holds the待ち合わせ場所 leaves the session
    /// without an IDE instead of failing the whole launch (the helper refuses a
    /// rendezvous the command may write, and refuses the command with it).
    #[test]
    fn a_project_folder_that_holds_the_rendezvous_is_noticed() {
        let rendezvous = "/root/.cache/zed-jp-enhance/ide/claude-1/bridge.sock";
        let held = |roots: &[&str]| {
            super::remote_launch::writable_root_holding(
                &roots.iter().map(PathBuf::from).collect::<Vec<_>>(),
                rendezvous,
            )
        };
        assert_eq!(held(&["/root"]), Some("/root".to_string()));
        assert_eq!(
            held(&["/srv/app", "/root/.cache"]),
            Some("/root/.cache".to_string())
        );
        assert_eq!(held(&["/srv/app"]), None);
        assert_eq!(held(&["/root/.cache/zed-jp-enhance-other"]), None);
        assert_eq!(held(&[]), None);
        // 綴りの揺れ (末尾の `/`・`//`・`/./`) でも同じ場所は同じ場所。名前はユーザーが
        // 開いたときの綴りのまま見せる。
        for spelling in ["/root/", "//root", "/root/.", "/./root//"] {
            assert_eq!(
                held(&[spelling]),
                Some(spelling.to_string()),
                "{spelling} holds the rendezvous"
            );
        }
        // 名前の一部が同じだけのものは別の場所。
        assert_eq!(held(&["/roo"]), None);
        assert_eq!(held(&["/root2"]), None);
        // 上を指す成分を含む綴りは読めないので、何も言わない (中継は接続先が断る)。
        assert_eq!(held(&["/root/../root"]), None);
    }

    /// One configuration per set of project folders, in whatever order they
    /// come.
    #[test]
    fn each_set_of_project_folders_has_its_own_key() {
        let one = PathBuf::from(path!("/work/one"));
        let two = PathBuf::from(path!("/work/two"));
        let three = PathBuf::from(path!("/work/three"));
        let key = project_key(&[one.clone(), two.clone()]);
        assert_eq!(key.len(), 16);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()), "{key}");
        assert_eq!(
            key,
            project_key(&[two.clone(), one.clone()]),
            "in any order"
        );
        assert_eq!(key, project_key(&[one.clone(), two.clone(), one.clone()]));
        assert_ne!(key, project_key(&[one.clone(), three]), "another folder");
        assert_ne!(
            key,
            project_key(std::slice::from_ref(&one)),
            "a folder fewer"
        );
        assert_ne!(project_key(std::slice::from_ref(&one)), project_key(&[two]));
        assert!(SessionLayout::from_rendezvous(RENDEZVOUS, SESSION_ID, &key, PORT).is_ok());
    }

    /// The configuration holds the session's login: every other sandboxed
    /// command — one working on an ordinary project — cannot read it, and what
    /// the helper protects in the session's own grant does not cover it.
    #[test]
    fn the_config_directory_is_hidden_from_other_commands() {
        let layout = layout();
        let home = Path::new("/root");
        let config_root = PathBuf::from(&layout.config_root);
        let config_dir = Path::new(&layout.config_dir);
        assert_eq!(config_dir.parent(), Some(config_root.as_path()));

        let worktree = PathBuf::from("/work");
        let hidden = sandbox::builtin_read_hidden_paths(home, std::slice::from_ref(&worktree));
        assert!(
            hidden.iter().any(|entry| config_dir.starts_with(entry)),
            "{hidden:?}"
        );
        for entry in sandbox::builtin_protected_paths(home, std::slice::from_ref(&config_root)) {
            assert!(
                !config_dir.starts_with(&entry),
                "{} covers the session's config",
                entry.display()
            );
        }
    }

    /// ADR-F55: both this session and the ACP Claude agent are the
    /// unmodified `claude` signing itself in through Anthropic's own flow, in
    /// the same project's sandbox, so they share one configuration folder and
    /// one sign-in — this test fails the moment the two drift apart.
    #[test]
    fn the_acp_claude_agent_shares_this_sessions_folder() {
        let layout = layout();
        let agent_config_dir = project::agent_sandbox::agent_config_dir(
            Path::new("/root"),
            PROJECT_KEY,
            &project::agent_sandbox::SandboxedAgentProfile::Claude,
        );
        assert_eq!(
            agent_config_dir.parent(),
            Some(Path::new(&layout.config_root)),
            "both folders live in the project's own directory"
        );
        assert_eq!(agent_config_dir, PathBuf::from(&layout.config_dir));
        assert!(
            layout
                .config_dir
                .ends_with(project::agent_sandbox::CLI_SESSION_FOLDER),
            "the two crates spell the folder alike: {}",
            layout.config_dir
        );
    }

    /// ADR-F55: the agent and the CLI session reach the same two hosts — the
    /// model API and the token endpoint the paste-a-code login ends at — so a
    /// sign-in that works in the CLI tab works for the agent too.
    #[test]
    fn the_acp_claude_agent_reaches_the_same_hosts_as_this_session() {
        let agent = project::agent_sandbox::SandboxedAgentProfile::Claude
            .hosts()
            .expect("the vendor's own hosts");
        assert_eq!(agent, &CLAUDE_HOSTS[..]);
    }

    #[test]
    fn the_outside_sandbox_confirmation_names_what_runs_where() {
        let local = outside_sandbox_detail(
            CLAUDE_PROGRAM,
            false,
            Some(NO_FOLDER_REASON),
            OutsideSandboxStart::Once,
        );
        assert!(local.starts_with(OUTSIDE_SANDBOX_NOTICE), "{local}");
        assert!(local.contains("`claude` on this PC"), "{local}");
        assert_eq!(
            local.lines().skip(1).collect::<Vec<_>>(),
            [
                NO_FOLDER_REASON,
                OUTSIDE_SANDBOX_LIMITS_LOCAL,
                OUTSIDE_SANDBOX_CLAUDE_LOGIN,
                OUTSIDE_SANDBOX_ONCE,
            ],
            "why, then what goes, what is shared and that it is this once"
        );

        let remote = outside_sandbox_detail(
            CLAUDE_PROGRAM,
            true,
            Some(REMOTE_CONNECTION_REASON),
            OutsideSandboxStart::Once,
        );
        assert!(
            remote.starts_with("Runs `claude` on the remote host"),
            "{remote}"
        );
        assert!(
            !remote.contains("this PC") && !remote.contains("Zed's own permissions"),
            "{remote}"
        );
        assert!(
            remote.contains(OUTSIDE_SANDBOX_LIMITS_REMOTE)
                && !remote.contains(OUTSIDE_SANDBOX_LIMITS_LOCAL),
            "nothing of this PC's reaches the host: {remote}"
        );
        assert!(!remote.contains(OUTSIDE_SANDBOX_NEW_SESSION), "{remote}");

        let restart = outside_sandbox_detail(
            agent_servers::CLAUDE_AGENT_ID,
            false,
            None,
            OutsideSandboxStart::RestartingThread,
        );
        assert!(
            restart.contains(OUTSIDE_SANDBOX_CLAUDE_LOGIN),
            "the ACP Claude shares the CLI's sign-in outside the sandbox: {restart}"
        );
        assert!(
            restart.ends_with(OUTSIDE_SANDBOX_NEW_SESSION),
            "the running conversation does not follow it out: {restart}"
        );

        assert_eq!(
            outside_sandbox_message(CLAUDE_PROGRAM),
            OUTSIDE_SANDBOX_MESSAGE
        );
        let message = outside_sandbox_message("codex");
        assert!(
            message.contains("`codex`") && !message.contains("Claude Code"),
            "{message}"
        );
        for on_remote_host in [false, true] {
            let detail =
                outside_sandbox_detail("codex", on_remote_host, None, OutsideSandboxStart::Once);
            assert!(
                detail.contains("`codex`") && !detail.contains("claude"),
                "{detail}"
            );
            assert!(detail.contains(OUTSIDE_SANDBOX_OTHER_LOGIN), "{detail}");
        }
    }

    #[test]
    fn the_hosts_consent_says_they_apply_to_the_whole_ide() {
        let detail = hosts_detail(&["api.anthropic.com".into()]);
        assert!(
            detail.contains(
                "added to the IDE-wide allowlist and also apply to the built-in agent's commands"
            ),
            "{detail}"
        );
        assert!(
            detail.ends_with(
                "Hosts that need to be allowed: api.anthropic.com (not added), platform.claude.com (added)"
            ),
            "{detail}"
        );
    }

    /// Only a launch still starting holds its configuration: once its
    /// terminal exists, the next launch starts another session beside it.
    #[test]
    fn sessions_on_one_configuration_run_side_by_side_once_started() {
        let starting = StartingSessions::default();
        let key = session_key(Some("Ubuntu"), PROJECT_KEY);
        let mut first = starting
            .begin(key.clone())
            .expect("the first launch starts");
        assert!(
            starting.begin(key.clone()).is_none(),
            "a launch still starting holds its configuration"
        );
        assert!(
            starting
                .begin(session_key(Some("ubuntu"), PROJECT_KEY))
                .is_none(),
            "the distro's name in another case is the same distro"
        );
        assert!(
            starting
                .begin(session_key(Some("Debian"), PROJECT_KEY))
                .is_some(),
            "another distro's configuration is another"
        );
        assert!(
            starting
                .begin(session_key(Some("Ubuntu"), "fedcba9876543210"))
                .is_some()
        );

        first.started();
        let mut second = starting
            .begin(key.clone())
            .expect("a second session starts while the first runs");
        second.started();
        let third = starting.begin(key.clone());
        assert!(third.is_some(), "and a third, while both run");
        drop(third);
        drop(first);
        drop(second);
        assert!(starting.begin(key).is_some());
    }

    /// A launch that has started gives its key back; when it is dropped
    /// later — its session's end, or the rest of the launch — it must not
    /// take the key from a launch that has begun since.
    #[test]
    fn a_started_launch_does_not_free_the_key_of_a_later_one() {
        let starting = StartingSessions::default();
        let key = session_key(Some("Ubuntu"), PROJECT_KEY);
        let mut first = starting
            .begin(key.clone())
            .expect("the first launch starts");
        first.started();
        let later = starting
            .begin(key.clone())
            .expect("the key is free once the first has started");
        drop(first);
        assert!(
            starting.begin(key.clone()).is_none(),
            "the later launch still starts, and still holds the key"
        );
        drop(later);
        assert!(
            starting.begin(key).is_some(),
            "a launch that ends before its terminal exists frees the key"
        );
    }

    #[test]
    fn the_plan_runs_the_staged_cli_with_its_ide_port() {
        let layout = layout();
        let root = PathBuf::from(path!("/project"));
        let plan = plan_session(
            &layout,
            RENDEZVOUS,
            CLAUDE_PATH,
            Some("high"),
            std::slice::from_ref(&root),
            PORT,
            false,
        )
        .expect("a valid plan");

        assert_eq!(plan.command.program, "/bin/sh");
        assert_eq!(plan.command.args.first().map(String::as_str), Some("-c"));
        let script = plan.command.args.get(1).expect("the launch script");
        assert!(
            script.contains("CLAUDE_CODE_TMPDIR=${TMPDIR:-/tmp}"),
            "{script}"
        );
        assert!(
            script.contains("USERPROFILE=$CLAUDE_CONFIG_DIR"),
            "{script}"
        );
        assert!(script.ends_with("exec \"$0\" \"$@\""), "{script}");
        assert_eq!(
            plan.command.args.get(2..),
            Some(
                &[
                    CLAUDE_PATH.to_string(),
                    "--effort".to_string(),
                    "high".to_string()
                ][..]
            )
        );
        assert_eq!(plan.command.cwd.as_deref(), Some(root.as_path()));

        let mut environment = plan
            .command
            .env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        environment.sort();
        assert_eq!(
            environment,
            [
                ("CLAUDE_CODE_DISABLE_ARTIFACT", "1"),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
                ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
                ("CLAUDE_CODE_IDE_HOST_OVERRIDE", "127.0.0.1"),
                ("CLAUDE_CODE_SSE_PORT", "45123"),
                (
                    "CLAUDE_CONFIG_DIR",
                    format!("{CONFIG_ROOT}/config").as_str()
                ),
                ("COLORTERM", "truecolor"),
                ("DISABLE_AUTOUPDATER", "1"),
                ("DISABLE_INSTALLATION_CHECKS", "1"),
                ("DISABLE_UPDATES", "1"),
                ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
                ("TERM", "xterm-256color"),
            ]
        );
        #[cfg(target_os = "windows")]
        for (name, _) in &environment {
            assert!(
                sandbox::is_forwardable_env_var(name),
                "{name} would not reach the CLI"
            );
        }

        assert_eq!(
            plan.writable_paths,
            [root.clone(), PathBuf::from(CONFIG_ROOT)]
        );
        assert_eq!(
            plan.allowed_hosts,
            ["api.anthropic.com", "platform.claude.com"]
        );
        assert_eq!(
            plan.ide_forward,
            sandbox::IdeForward {
                inner_port: PORT,
                rendezvous_path: RENDEZVOUS.to_string(),
            }
        );

        let without_effort = plan_session(
            &layout,
            RENDEZVOUS,
            CLAUDE_PATH,
            None,
            std::slice::from_ref(&root),
            PORT,
            false,
        )
        .expect("a valid plan");
        assert_eq!(
            without_effort.command.args.get(2..),
            Some(&[CLAUDE_PATH.to_string()][..])
        );
        assert!(
            plan_session(&layout, RENDEZVOUS, "claude", None, &[root], PORT, false).is_err(),
            "the CLI is never looked up on PATH"
        );
        assert!(plan_session(&layout, RENDEZVOUS, CLAUDE_PATH, None, &[], PORT, false).is_err());
    }

    /// A session on an SSH host is kept out of the cache the CLI it runs is
    /// staged in, under this build's name and under the one builds from
    /// before the rename stage theirs in; spelled out so that dropping either
    /// fails here.
    #[test]
    fn a_remote_session_is_kept_out_of_both_host_caches() {
        assert_eq!(
            super::remote_launch::host_cache_protected_paths("/home/ubuntu"),
            [
                PathBuf::from("/home/ubuntu/.cache/forgekairos"),
                PathBuf::from("/home/ubuntu/.cache/zed-jp-enhance"),
            ]
        );
    }

    /// 第 3 段: the session on an SSH host is placed by the same rule as the
    /// WSL one — the account's home decides, and the project's key names the
    /// folder — so the two never see each other's configuration. ADR-F53: the
    /// environment points the CLI at an IDE only when there is one to point it
    /// at; a session whose bridge would not start runs as it does in a plain
    /// terminal instead of waiting for a lock file that never arrives.
    #[test]
    fn a_remote_session_is_placed_like_the_wsl_one() {
        let (config_root, config_dir) =
            config_paths("/home/ubuntu", PROJECT_KEY).expect("a valid placement");
        assert_eq!(
            config_root,
            format!(
                "/home/ubuntu/{}/0123456789abcdef",
                sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE
            )
        );
        assert_eq!(config_dir, format!("{config_root}/config"));
        // The same rule the rendezvous-derived layout applies, so the WSL
        // session and an SSH one with the same home land in the same place.
        assert_eq!(layout().config_dir, format!("{CONFIG_ROOT}/config"));
        assert_eq!(
            config_paths("/root", PROJECT_KEY)
                .expect("a valid placement")
                .1,
            layout().config_dir
        );
        for bad_home in ["root", "/root/", "/root\n", ""] {
            assert!(config_paths(bad_home, PROJECT_KEY).is_err(), "{bad_home:?}");
        }
        assert!(config_paths("/root", "not a key").is_err());

        let with_ide = session_environment(&config_dir, Some(45123), false);
        let without_ide = session_environment(&config_dir, None, false);
        let names = |environment: &[(String, String)]| {
            let mut names = environment
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        // Nothing points the CLI at an IDE when there is none to point it at:
        // with the port set it would wait for a lock file that never arrives
        // (ADR-F53: the IDE bridge failed to start).
        assert!(
            !names(&without_ide).contains(&"CLAUDE_CODE_SSE_PORT".to_string()),
            "{without_ide:?}"
        );
        assert!(
            !names(&without_ide).contains(&"CLAUDE_CODE_IDE_HOST_OVERRIDE".to_string()),
            "{without_ide:?}"
        );
        assert_eq!(
            names(&with_ide)
                .into_iter()
                .filter(|name| !name.starts_with("CLAUDE_CODE_SSE")
                    && !name.starts_with("CLAUDE_CODE_IDE"))
                .collect::<Vec<_>>(),
            names(&without_ide),
            "the rest of the environment is the same either way"
        );
        assert_eq!(
            without_ide
                .iter()
                .find(|(name, _)| name == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| value.as_str()),
            Some(config_dir.as_str())
        );
    }

    /// ADR-F50 on an SSH host: the folders and the configuration are the
    /// host's own paths, which is how its worktrees are spelled, so the scope
    /// is exactly those — there is no second spelling as there is in WSL.
    #[test]
    fn a_remote_session_holds_its_folders_and_its_configuration() {
        let roots = [PathBuf::from("/srv/app"), PathBuf::from("/srv/lib")];
        assert_eq!(
            remote_session_write_scope(&roots, CONFIG_ROOT),
            [
                PathBuf::from("/srv/app"),
                PathBuf::from("/srv/lib"),
                PathBuf::from(CONFIG_ROOT),
            ]
        );
    }

    /// ADR-F56: with the IDE tools on, the CLI gets the `zed` server on its IDE
    /// channel's `/mcp/zed` with every assist tool allowed up front, and the token
    /// only by name — the CLI expands it from what the launch script reads out
    /// of the lock file — never on its command line. Off, none of it.
    #[test]
    fn the_ide_tools_reach_the_sandboxed_cli_without_a_token_on_its_command_line() {
        let (_, args) =
            launch_invocation(CLAUDE_PATH, Some("high"), Some(PORT)).expect("an absolute path");
        let script = args.get(1).expect("the launch script");
        let cli_args = args.get(2..).expect("the CLI and its arguments");
        assert_eq!(cli_args.first().map(String::as_str), Some(CLAUDE_PATH));
        let value_of = |option: &str| {
            let index = cli_args
                .iter()
                .position(|argument| argument == option)
                .unwrap_or_else(|| panic!("{option} is passed: {cli_args:?}"));
            cli_args
                .get(index + 1)
                .unwrap_or_else(|| panic!("{option} has a value"))
                .clone()
        };
        let config: serde_json::Value =
            serde_json::from_str(&value_of("--mcp-config")).expect("the config is JSON");
        let server = &config["mcpServers"][ide_server::MCP_SERVER_NAME];
        assert_eq!(server["type"], "http");
        assert_eq!(
            server["url"],
            format!("http://127.0.0.1:{PORT}{}", ide_server::ASSIST_MCP_PATH)
        );
        assert_eq!(
            server["headers"]["Authorization"],
            format!("${{{}}}", ide_server::ASSIST_BEARER_ENV_VAR)
        );
        let allowed = value_of("--allowedTools");
        let mut allowed = allowed.split(',').collect::<Vec<_>>();
        allowed.sort();
        let mut expected = ide_server::ASSIST_TOOL_NAMES
            .iter()
            .map(|name| format!("mcp__{}__{name}", ide_server::MCP_SERVER_NAME))
            .collect::<Vec<_>>();
        expected.sort();
        assert_eq!(allowed, expected);
        assert_eq!(
            cli_args.get(cli_args.len() - 2..),
            Some(&["--effort".to_string(), "high".to_string()][..]),
            "nothing follows the effort"
        );
        // The script is what reads the token, under the very name the JSON uses,
        // and only when asked to.
        assert!(
            script.contains(&format!("{}=", ide_server::ASSIST_BEARER_ENV_VAR)),
            "{script}"
        );
        assert!(script.contains("authToken"), "{script}");
        assert!(script.contains("ZED_IDE_ASSIST"), "{script}");
        // The variable is the whole header value, so that the JSON has no blank
        // for a shell (Windows PowerShell and npm's `claude.ps1`) to split it at.
        assert!(
            script.contains(&format!(
                "{0}=\"Bearer ${0}\"",
                ide_server::ASSIST_BEARER_ENV_VAR
            )),
            "{script}"
        );
        assert!(
            !value_of("--mcp-config").contains(char::is_whitespace),
            "{cli_args:?}"
        );

        let (_, without) = launch_invocation(CLAUDE_PATH, None, None).expect("an absolute path");
        assert_eq!(without.get(2..), Some(&[CLAUDE_PATH.to_string()][..]));

        let root = PathBuf::from(path!("/project"));
        let environment = |assist_tools: bool| {
            let plan = plan_session(
                &layout(),
                RENDEZVOUS,
                CLAUDE_PATH,
                None,
                std::slice::from_ref(&root),
                PORT,
                assist_tools,
            )
            .expect("a valid plan");
            let has_config = plan
                .command
                .args
                .iter()
                .any(|argument| argument == "--mcp-config");
            (plan.command.env, has_config)
        };
        let (with_tools, with_config) = environment(true);
        assert!(with_config);
        assert_eq!(
            with_tools.get("ZED_IDE_ASSIST").map(String::as_str),
            Some("1")
        );
        #[cfg(target_os = "windows")]
        assert!(sandbox::is_forwardable_env_var("ZED_IDE_ASSIST"));
        let (without_tools, without_config) = environment(false);
        assert!(!without_config);
        assert!(!without_tools.contains_key("ZED_IDE_ASSIST"));
        assert!(
            !session_environment(CONFIG_ROOT, None, true)
                .iter()
                .any(|(name, _)| name == "ZED_IDE_ASSIST"),
            "no IDE channel, no token to read"
        );
    }

    /// The launch itself is shared: the shell that sets what only exists inside
    /// the sandbox and then becomes the CLI by absolute path, never by `PATH`.
    #[test]
    fn the_launch_never_looks_the_cli_up_on_path() {
        let (program, args) =
            launch_invocation(CLAUDE_PATH, Some("high"), None).expect("an absolute path");
        assert_eq!(program, "/bin/sh");
        assert_eq!(args.first().map(String::as_str), Some("-c"));
        assert_eq!(
            args.get(2..),
            Some(
                &[
                    CLAUDE_PATH.to_string(),
                    "--effort".to_string(),
                    "high".to_string(),
                ][..]
            )
        );
        assert_eq!(
            launch_invocation(CLAUDE_PATH, None, None)
                .expect("an absolute path")
                .1
                .get(2..),
            Some(&[CLAUDE_PATH.to_string()][..])
        );
        for bad in ["claude", "~/claude", "/tmp/cl\naude"] {
            assert!(launch_invocation(bad, None, None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_sweep_only_takes_our_leftovers_whose_bridge_is_gone() {
        let layout = layout();
        let bridged = "claude-11111111111111111111111111111111";
        let connected = "claude-22222222222222222222222222222222";
        let abandoned = "claude-33333333333333333333333333333333";
        let base = &layout.rendezvous_directory;
        let locks = &layout.lock_directory;
        let listing = format!(
            "zed-sweep-dir {bridged}\n\
             zed-sweep-dir {connected}\n\
             zed-sweep-dir {abandoned}\n\
             zed-sweep-dir zed-f49s-e2e-3892141\n\
             zed-sweep-dir claude-NOTHEX\n\
             zed-sweep-lock 45123.lock\n\
             zed-sweep-lock 4512.lock\n\
             zed-sweep-lock 50000.lock\n\
             zed-sweep-lock 99999.lock\n\
             zed-sweep-lock notes.txt\n\
             zed-sweep-lock 50000.lock.bak\n\
             zed-sweep-cmd /proc/self/fd/3 --ide-bridge --rendezvous {base}/{bridged}/bridge.sock --lock-file {locks}/45123.lock \n\
             zed-sweep-cmd /proc/self/fd/3 --remote-sandbox-helper --wsl --interactive --ide-listen 40000 --ide-rendezvous {base}/{connected}/bridge.sock -- /bin/sh\n"
        );
        assert_eq!(
            stale_entries(&listing, &layout),
            StaleEntries {
                rendezvous: vec![abandoned.to_string()],
                locks: vec!["4512.lock".to_string(), "50000.lock".to_string()],
            }
        );
        assert!(stale_entries("", &layout).is_empty());
    }

    #[test]
    fn a_refused_sandbox_is_explained_from_the_screen() {
        let screen = "\nzed-wsl-sandbox: ide-forward-invalid: the rendezvous is not private\n";
        let message = helper_failure_message(Some(126), screen).expect("the helper's signature");
        assert!(message.contains("carries the IDE integration"), "{message}");
        assert!(helper_failure_message(Some(0), screen).is_none());
        assert!(
            helper_failure_message(Some(126), "claude: zed-wsl-sandbox: x: y").is_none(),
            "only a marker at the start of a line counts"
        );
    }

    /// ADR-F50: what the session can write, as worktrees spell it.
    #[test]
    fn the_session_writes_its_folders_and_its_configuration() {
        let root = PathBuf::from(path!("/project"));
        assert_eq!(
            session_write_scope(std::slice::from_ref(&root), CONFIG_ROOT, Some("Ubuntu")),
            [
                root.clone(),
                PathBuf::from(format!(
                    r"\\wsl.localhost\Ubuntu\root\{}\0123456789abcdef",
                    sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE.replace('/', r"\")
                )),
            ]
        );
        assert_eq!(
            session_write_scope(std::slice::from_ref(&root), CONFIG_ROOT, None),
            [root],
            "without the distro's name the configuration has no host spelling"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            workspace::AppState::test(cx);
            editor::init(cx);
            ide_server::init(cx);
            // The pause is off by default (ADR-F50); these tests are about it.
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .agent
                        .get_or_insert_default()
                        .pause_worktree_code_after_agent_writes = Some(true);
                });
            });
        });
    }

    async fn open_workspace(
        roots: &[&Path],
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<Workspace>, VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/test"), json!({ "main.rs": "fn main() {}\n" }))
            .await;
        let project = Project::test(fs, roots.iter().copied(), cx).await;
        let window = cx.add_window({
            let project = project.clone();
            move |window, cx| MultiWorkspace::test_new(project, window, cx)
        });
        let workspace = window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("the window is open");
        let cx = VisualTestContext::from_window(window.into(), cx);
        (project, workspace, cx)
    }

    fn recording_launcher() -> (HostLauncher, Rc<RefCell<Vec<Option<String>>>>) {
        let launches = Rc::new(RefCell::new(Vec::new()));
        let launcher: HostLauncher = Rc::new({
            let launches = launches.clone();
            move |_workspace, effort, _window, _cx| launches.borrow_mut().push(effort)
        });
        (launcher, launches)
    }

    fn register_and_focus(
        workspace: &Entity<Workspace>,
        launcher: HostLauncher,
        cx: &mut VisualTestContext,
    ) {
        workspace.update_in(cx, |workspace, window, cx| {
            register_launch_actions(workspace, launcher);
            let pane = workspace.active_pane().focus_handle(cx);
            window.focus(&pane, cx);
        });
        cx.run_until_parked();
        // ADR-F92: these tests are about the starts outside, which are only
        // offered once the user allows running outside the sandbox. Set once
        // the workspace has settled, which reads the (empty) settings file.
        set_user_settings(OUTSIDE_ALLOWED, cx);
    }

    #[gpui::test]
    async fn launching_outside_the_sandbox_is_confirmed_every_time(cx: &mut TestAppContext) {
        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);

        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox { effort: None });
        cx.run_until_parked();
        let (message, detail) = cx.pending_prompt().expect("the launch asks first");
        assert_eq!(message, OUTSIDE_SANDBOX_MESSAGE);
        assert!(detail.contains(OUTSIDE_SANDBOX_NOTICE), "{detail}");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(launches.borrow().is_empty(), "declining starts nothing");

        for launch in 1..=2 {
            cx.dispatch_action(LaunchClaudeCodeOutsideSandbox {
                effort: Some("high".to_string()),
            });
            cx.run_until_parked();
            assert!(cx.has_pending_prompt(), "launch {launch} asks again");
            cx.simulate_prompt_answer("Start (This Time Only)");
            cx.run_until_parked();
            assert_eq!(launches.borrow().len(), launch);
        }
        assert_eq!(launches.borrow().last(), Some(&Some("high".to_string())));
    }

    fn set_user_settings(json: &str, cx: &mut VisualTestContext) {
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(json, cx)
                    .result()
                    .expect("valid user settings");
            });
        });
        cx.run_until_parked();
    }

    const OUTSIDE_ALLOWED: &str =
        r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } } }"#;
    const ALWAYS_OUTSIDE: &str = r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true }, "claude_code_run_outside_sandbox": true } }"#;

    /// ADR-F92: while running outside the sandbox is not allowed, the start
    /// outside asks nothing and starts nothing — it says so, with the way to
    /// the setting — and the "always outside" setting is ignored.
    #[gpui::test]
    async fn nothing_starts_outside_while_it_is_not_allowed(cx: &mut TestAppContext) {
        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);

        for settings in [
            "{}",
            r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": false }, "claude_code_run_outside_sandbox": true } }"#,
        ] {
            set_user_settings(settings, &mut cx);
            cx.dispatch_action(LaunchClaudeCodeOutsideSandbox {
                effort: Some("high".to_string()),
            });
            cx.run_until_parked();
            let (message, detail) = cx.pending_prompt().expect("the refusal is shown");
            assert_eq!(message, OUTSIDE_SANDBOX_NOT_ALLOWED_TITLE, "{settings}");
            assert_eq!(detail, OUTSIDE_SANDBOX_NOT_ALLOWED_DETAIL);
            cx.simulate_prompt_answer("Close");
            cx.run_until_parked();
            assert!(!cx.has_pending_prompt(), "nothing asked after the refusal");
            assert!(launches.borrow().is_empty(), "{settings}");
        }

        // The failed launch's notification leads to the setting instead.
        let failure = SandboxLaunchFailure {
            message: String::new(),
            effort: None,
            outside_sandbox_allowed: false,
        };
        let action = workspace::workspace_error::WorkspaceError::primary_action(&failure);
        assert_eq!(action.label.as_ref(), OPEN_SANDBOX_SETTINGS_LABEL);

        set_user_settings(OUTSIDE_ALLOWED, &mut cx);
        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox { effort: None });
        cx.run_until_parked();
        let (message, _) = cx.pending_prompt().expect("asked once it is allowed");
        assert_eq!(message, OUTSIDE_SANDBOX_MESSAGE);
        cx.simulate_prompt_answer("Start (This Time Only)");
        cx.run_until_parked();
        assert_eq!(*launches.borrow(), [None]);
    }

    /// ADR-F49 決定 19: with the user's own choice to start the CLI outside
    /// every time, neither launch asks — and turning it back off brings the
    /// question back.
    #[gpui::test]
    async fn the_users_choice_to_start_outside_is_not_asked_again(cx: &mut TestAppContext) {
        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);
        set_user_settings(ALWAYS_OUTSIDE, &mut cx);

        cx.dispatch_action(LaunchClaudeCode {
            effort: Some("high".to_string()),
        });
        cx.run_until_parked();
        assert!(
            !cx.has_pending_prompt(),
            "the user already said yes to every start"
        );
        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox { effort: None });
        cx.run_until_parked();
        assert!(!cx.has_pending_prompt());
        assert_eq!(*launches.borrow(), [Some("high".to_string()), None]);

        set_user_settings(OUTSIDE_ALLOWED, &mut cx);
        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox { effort: None });
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "asked again once the choice is gone"
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(launches.borrow().len(), 2);
    }

    /// ADR-F49 決定 19 / ADR-F50: a project's settings cannot let the CLI out
    /// of its sandbox — the session that would be let out can write them.
    #[gpui::test]
    async fn a_projects_settings_cannot_start_the_cli_outside_without_asking(
        cx: &mut TestAppContext,
    ) {
        let (project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);
        let worktree_id = project.read_with(&cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the folder is open")
                .read(cx)
                .id()
        });
        cx.update(|_window, cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        worktree_id,
                        settings::LocalSettingsPath::InWorktree(
                            util::rel_path::RelPath::empty().into(),
                        ),
                        settings::LocalSettingsKind::Settings,
                        Some(ALWAYS_OUTSIDE),
                        cx,
                    )
                    .ok();
            });
        });
        cx.run_until_parked();

        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox { effort: None });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "still asked");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(launches.borrow().is_empty());
    }

    /// ADR-F49 決定 19: a sandboxed launch that fails says so with a way out —
    /// the start outside the sandbox, still confirmed, with the same effort.
    #[gpui::test]
    async fn a_failed_sandboxed_launch_offers_the_start_outside_after_asking(
        cx: &mut TestAppContext,
    ) {
        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);

        workspace.update(&mut cx, |workspace, cx| {
            show_launch_failure(
                workspace,
                "Could not start Claude Code (sandbox): WSL is not installed".to_string(),
                Some("high".to_string()),
                cx,
            );
        });
        cx.run_until_parked();
        let notification_id =
            workspace::notifications::NotificationId::unique::<SandboxLaunchFailure>();
        assert!(workspace.read_with(&cx, |workspace, _| {
            workspace.notification_ids().contains(&notification_id)
        }));

        let failure = SandboxLaunchFailure {
            message: String::new(),
            effort: Some("high".to_string()),
            outside_sandbox_allowed: true,
        };
        let action = workspace::workspace_error::WorkspaceError::primary_action(&failure);
        assert_eq!(action.label.as_ref(), "Start Outside the Sandbox…");
        let workspace::workspace_error::ErrorActionHandler::Action(action) = action.handler else {
            panic!("the button starts something");
        };
        // What the notification's button does when it is clicked.
        cx.update(|window, cx| window.dispatch_action(action.boxed_clone(), cx));
        cx.run_until_parked();
        assert!(
            cx.has_pending_prompt(),
            "the start outside is still confirmed"
        );
        assert!(launches.borrow().is_empty());
        cx.simulate_prompt_answer("Start (This Time Only)");
        cx.run_until_parked();
        assert_eq!(*launches.borrow(), [Some("high".to_string())]);
    }

    /// ADR-F53: エージェントパネルが無い (`disable_ai` で外されている、
    /// パネル未登録) ときは、今までどおりターミナルパネルに開く。CLI は人間の
    /// 操作 (ADR-F38) なので AI が無効でも使えなければならない。
    #[gpui::test]
    async fn without_an_agent_panel_the_cli_still_opens_in_the_terminal_panel(
        cx: &mut TestAppContext,
    ) {
        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let (fallback, fallbacks) = recording_launcher();
        // 置き場の判断だけを見る: 本物の起動は `claude` を本当に動かすので、
        // パネルが無いときに通る経路を記録するものに差し替える。
        let launcher: HostLauncher = Rc::new(move |workspace, effort, window, cx| {
            let fallback = fallback.clone();
            super::launch_on_host_in_tab(
                workspace,
                effort,
                &move |workspace, effort, window, cx, _ide_tools| {
                    fallback(workspace, effort, window, cx)
                },
                window,
                cx,
            );
        });
        register_and_focus(&workspace, launcher, &mut cx);

        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox {
            effort: Some("high".to_string()),
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "起動のたびに確認する");
        cx.simulate_prompt_answer("Start (This Time Only)");
        cx.run_until_parked();

        assert_eq!(
            *fallbacks.borrow(),
            [Some("high".to_string())],
            "パネルが無ければターミナルパネル経路へ、引数もそのまま"
        );
    }

    #[gpui::test]
    async fn a_launch_while_one_still_starts_says_so_and_goes_away_by_itself(
        cx: &mut TestAppContext,
    ) {
        use super::session::{STILL_STARTING_MESSAGE, begin_session, still_starting_id};

        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let weak_workspace = workspace.downgrade();
        let key = "ubuntu/0123456789abcdef".to_string();
        let says_still_starting = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.notification_ids().contains(&still_starting_id())
            })
        };

        let mut first = cx
            .update(|_, cx| begin_session(key.clone(), &weak_workspace, cx))
            .expect("the first launch starts");
        assert!(!says_still_starting(&mut cx));
        let second = cx.update(|_, cx| begin_session(key.clone(), &weak_workspace, cx));
        assert!(
            second.is_none(),
            "a launch while the first still starts does nothing"
        );
        assert!(says_still_starting(&mut cx));
        assert_eq!(
            STILL_STARTING_MESSAGE,
            "Claude Code (sandbox) is still starting for this project. Try again once it has started."
        );

        cx.executor()
            .advance_clock(std::time::Duration::from_secs(6));
        cx.run_until_parked();
        assert!(
            !says_still_starting(&mut cx),
            "the notice hides itself after a few seconds"
        );

        first.started();
        let third = cx.update(|_, cx| begin_session(key, &weak_workspace, cx));
        assert!(
            third.is_some(),
            "once the first session's terminal exists, another one starts"
        );
        assert!(!says_still_starting(&mut cx));
    }

    #[gpui::test]
    async fn a_project_without_a_folder_launches_outside_the_sandbox_after_asking(
        cx: &mut TestAppContext,
    ) {
        let (_project, workspace, mut cx) = open_workspace(&[], cx).await;
        let (launcher, launches) = recording_launcher();
        register_and_focus(&workspace, launcher, &mut cx);

        cx.dispatch_action(LaunchClaudeCode { effort: None });
        cx.run_until_parked();
        let (_message, detail) = cx.pending_prompt().expect("the launch asks first");
        let reason = if cfg!(target_os = "windows") {
            NO_FOLDER_REASON
        } else {
            UNSUPPORTED_OS_REASON
        };
        assert!(detail.contains(reason), "{detail}");
        cx.simulate_prompt_answer("Start (This Time Only)");
        cx.run_until_parked();
        assert_eq!(*launches.borrow(), [None]);
    }

    /// ADR-F94: Claude Code picked for a project (the sidebar's menu names
    /// one) starts in that project, not in the one the window last drew.
    #[gpui::test]
    async fn claude_code_picked_for_a_project_starts_in_that_project(cx: &mut TestAppContext) {
        let (_project, workspace_with_folder, mut cx) =
            open_workspace(&[path!("/test").as_ref()], cx).await;
        let project_without_folder = Project::test(FakeFs::new(cx.executor()), [], &mut cx).await;
        let multi_workspace = cx
            .update(|window, _| window.root::<MultiWorkspace>())
            .flatten()
            .expect("the window shows a multi-workspace");
        let workspace_without_folder =
            multi_workspace.update_in(&mut cx, |multi_workspace, window, cx| {
                let added = multi_workspace.test_add_workspace(project_without_folder, window, cx);
                multi_workspace.activate(workspace_with_folder.clone(), None, window, cx);
                added
            });
        cx.run_until_parked();
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        set_user_settings(OUTSIDE_ALLOWED, &mut cx);

        cx.update(|window, cx| {
            crate::ai_choices::open_ai_choice(
                &crate::ai_choices::AiChoiceTarget::LaunchClaudeCode,
                &workspace_without_folder.downgrade(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let (_message, detail) = cx
            .pending_prompt()
            .expect("the project without a folder asks before starting outside");
        let reason = if cfg!(target_os = "windows") {
            NO_FOLDER_REASON
        } else {
            UNSUPPORTED_OS_REASON
        };
        assert!(detail.contains(reason), "{detail}");
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
    }

    async fn resume(
        execution: &Entity<WorktreeCodeExecution>,
        worktree_id: WorktreeId,
        cx: &mut VisualTestContext,
    ) -> ResumeOutcome {
        execution
            .update(cx, |execution, cx| {
                let generation = execution
                    .generation(worktree_id)
                    .expect("the folder is known");
                execution.resume(worktree_id, generation, cx)
            })
            .await
    }

    /// ADR-F50: a session is its folders' writer from before its CLI starts
    /// to its end: they stay paused, and resuming them waits for that end.
    #[gpui::test]
    async fn a_running_session_keeps_its_folders_paused_until_it_ends(cx: &mut TestAppContext) {
        let (project, _workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        cx.run_until_parked();
        let (worktree_id, execution) = project.read_with(&cx, |project, cx| {
            let worktree = project.worktrees(cx).next().expect("the folder is open");
            (
                worktree.read(cx).id(),
                project.worktree_code_execution().clone(),
            )
        });
        let state = |cx: &mut VisualTestContext| {
            execution.read_with(cx, |execution, _| execution.state(worktree_id))
        };
        assert_eq!(state(&mut cx), WorktreeCodeState::Allowed);

        let scope = session_write_scope(
            &[PathBuf::from(path!("/test"))],
            CONFIG_ROOT,
            Some("Ubuntu"),
        );
        let (writer, resolved) = cx.update(|_, cx| register_session_writer(&project, scope, cx));
        resolved.await;
        assert_eq!(
            state(&mut cx),
            WorktreeCodeState::Paused,
            "paused before the CLI starts"
        );
        assert_eq!(
            resume(&execution, worktree_id, &mut cx).await,
            ResumeOutcome::WriterRunning,
            "not while the session runs"
        );
        assert_eq!(state(&mut cx), WorktreeCodeState::Paused);

        drop(writer);
        cx.run_until_parked();
        assert_eq!(
            resume(&execution, worktree_id, &mut cx).await,
            ResumeOutcome::Resumed
        );
        assert_eq!(state(&mut cx), WorktreeCodeState::Allowed);
    }

    /// One direction of an in-memory byte pipe.
    #[derive(Clone)]
    struct PipeWriter(mpsc::UnboundedSender<Vec<u8>>);

    struct PipeReader {
        receiver: mpsc::UnboundedReceiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    fn pipe() -> (PipeWriter, PipeReader) {
        let (sender, receiver) = mpsc::unbounded();
        (
            PipeWriter(sender),
            PipeReader {
                receiver,
                pending: Vec::new(),
            },
        )
    }

    impl AsyncWrite for PipeWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.0.unbounded_send(buf.to_vec()) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            }
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            self.0.close_channel();
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for PipeReader {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            while this.pending.is_empty() {
                match this.receiver.poll_next_unpin(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(chunk)) => this.pending = chunk,
                    Poll::Ready(None) => return Poll::Ready(Ok(0)),
                }
            }
            let count = this.pending.len().min(buf.len());
            buf[..count].copy_from_slice(&this.pending[..count]);
            this.pending.drain(..count);
            Poll::Ready(Ok(count))
        }
    }

    /// The CLI's end of one IDE connection, carried in bridge frames.
    struct FramedClient {
        connection_id: u32,
        to_mux: PipeWriter,
        from_mux: PipeReader,
    }

    impl AsyncWrite for FramedClient {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut encoded = Vec::new();
            sandbox::encode_frame(
                &sandbox::BridgeFrame::data(self.connection_id, buf.to_vec()),
                &mut encoded,
            );
            match self.to_mux.0.unbounded_send(encoded) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(_) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            }
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncRead for FramedClient {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().from_mux).poll_read(cx, buf)
        }
    }

    fn upgrade_request(token: &str) -> async_tungstenite::tungstenite::http::Request<()> {
        async_tungstenite::tungstenite::http::Request::builder()
            .method("GET")
            .uri("ws://127.0.0.1:45123/")
            .header("Host", "127.0.0.1:45123")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("Sec-WebSocket-Protocol", "mcp")
            .header("X-Claude-Code-Ide-Authorization", token)
            .body(())
            .expect("building the upgrade request")
    }

    async fn request(
        socket: &mut WebSocketStream<FramedClient>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        socket
            .send(Message::text(request.to_string()))
            .await
            .expect("sending a request");
        loop {
            let frame = socket
                .next()
                .await
                .expect("the session keeps the connection open")
                .expect("the frame is valid");
            let Ok(text) = frame.to_text() else {
                continue;
            };
            let message: Value = serde_json::from_str(text).expect("the frame is JSON");
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
        }
    }

    fn is_connected(project: &Entity<Project>, cx: &mut VisualTestContext) -> bool {
        let project_id = project.entity_id();
        cx.update(|_, cx| zed_actions::ide_server::is_claude_code_connected(project_id, cx))
    }

    #[gpui::test]
    async fn bridge_frames_reach_the_sandbox_ide_session(cx: &mut TestAppContext) {
        let (project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        cx.run_until_parked();
        let mapper: Arc<dyn ide_server::PathMapper> = if cfg!(target_os = "windows") {
            Arc::new(ide_server::WslPathMapper {
                distro: "Ubuntu".to_string(),
            })
        } else {
            Arc::new(ide_server::IdentityPathMapper)
        };
        let (pending, auth_token) = cx.update(|_, cx| {
            PendingIdeSession::start(
                workspace.downgrade(),
                mapper,
                vec![PathBuf::from(path!("/test"))],
                "claude-00000000000000000000000000000000",
                cx,
            )
        });

        // The bridge's stdout (frames to the mux) and stdin (frames from it).
        let (to_mux, bridge_stdout) = pipe();
        let (bridge_stdin, mut from_mux) = pipe();
        let link = cx.update(|_, cx| pending.attach(bridge_stdout, bridge_stdin, cx));

        // The bridge's side of connection 1.
        let (client_input, client_output) = pipe();
        let stdin_closed = Arc::new(AtomicBool::new(false));
        let _bridge = cx.executor().spawn({
            let stdin_closed = stdin_closed.clone();
            async move {
                let mut decoder = sandbox::BridgeFrameDecoder::new();
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = match from_mux.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    let frames = decoder.push(&buffer[..read]).expect("valid frames");
                    for frame in frames {
                        if frame.kind == sandbox::BridgeFrameKind::Data && frame.connection_id == 1
                        {
                            client_input.0.unbounded_send(frame.payload).ok();
                        }
                    }
                }
                stdin_closed.store(true, Ordering::SeqCst);
            }
        });
        let mut open = Vec::new();
        sandbox::encode_frame(&sandbox::BridgeFrame::open(1), &mut open);
        to_mux.0.unbounded_send(open).expect("the mux reads");

        let client = FramedClient {
            connection_id: 1,
            to_mux: to_mux.clone(),
            from_mux: client_output,
        };
        let (mut socket, _response) =
            async_tungstenite::client_async(upgrade_request(&auth_token), client)
                .await
                .expect("the session accepts its own token");
        cx.run_until_parked();
        assert!(
            is_connected(&project, &mut cx),
            "the sandboxed CLI lights up the connected flag"
        );

        let response = request(
            &mut socket,
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "claude-code", "version": "2.1.270" }
            }),
        )
        .await;
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            json!(branding::CURRENT.product_name)
        );
        let response = request(&mut socket, 2, "tools/list", json!({})).await;
        let mut names = response["result"]["tools"]
            .as_array()
            .expect("tools is an array")
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "closeAllDiffTabs",
                "close_tab",
                "getDiagnostics",
                "openDiff"
            ]
        );

        drop(link);
        cx.run_until_parked();
        // A released entity is cleaned up at the end of the next update.
        cx.update(|_, _| {});
        cx.run_until_parked();
        assert!(
            stdin_closed.load(Ordering::SeqCst),
            "ending the session ends the bridge's stdin"
        );
        assert!(!is_connected(&project, &mut cx));
        assert!(
            !matches!(socket.next().await, Some(Ok(_))),
            "the CLI's connection is closed"
        );
    }

    /// Stages a Claude Code in WSL by hand, for a manual end-to-end check that
    /// should not download it again. Set `ZED_E2E_CLAUDE_BINARY` to a Linux
    /// `claude` verified against Anthropic's signed manifest and
    /// `ZED_E2E_CLAUDE_SHA256` to its digest: after its consent and the
    /// manifest's check, `ensure_managed_claude_binary` finds it staged.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn stage_claude_for_manual_testing() {
        let binary = std::env::var("ZED_E2E_CLAUDE_BINARY").expect("ZED_E2E_CLAUDE_BINARY");
        let sha256 = std::env::var("ZED_E2E_CLAUDE_SHA256").expect("ZED_E2E_CLAUDE_SHA256");
        let staged = futures::executor::block_on(sandbox::stage_agent_binary(
            None,
            "claude",
            Path::new(&binary),
            &sha256,
        ))
        .expect("staging the binary");
        println!("staged at {staged}");
    }
}
