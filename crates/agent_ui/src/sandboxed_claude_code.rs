//! Claude Code CLI の起動導線 (ADR-F49 決定 1・3・6)。
//!
//! 「Claude Code を起動」は、Windows のローカルのプロジェクトではサンドボックス (WSL のヘルパー) の
//! 対話セッションとして CLI を起動し、IDE 連携はヘルパーの IDE 転送 → ブリッジ → サンドボックス用の
//! IDE セッションで繋ぐ。それ以外 (リモート・共同編集・Windows 以外・フォルダーなし) と
//! 「サンドボックスの外で起動」は、この PC の `claude` を起動のたびに確認してから起動する。
//!
//! ハンドラーが `terminal_view` ではなくここにあるのは、IDE セッション (`ide_server`) と Linux 版の
//! CLI の取得 (F49-L1) を使うため。`terminal_view` はどちらにも依存できない (`agent_ui` が
//! `terminal_view` に依存している)。

use std::path::PathBuf;
use std::rc::Rc;

use gpui::{App, Context, PromptButton, PromptLevel, Window};
use project::Project;
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::terminal::{LaunchClaudeCode, LaunchClaudeCodeOutsideSandbox};

/// The program the launch actions start.
const CLAUDE_PROGRAM: &str = "claude";
const OUTSIDE_SANDBOX_MESSAGE: &str = "Claude Code をサンドボックスの外で起動しますか？";
/// ADR-F46 A-3 の帯と同じ言い回し。
const OUTSIDE_SANDBOX_NOTICE: &str = "サンドボックスの外 (Zed 本体の権限) で実行されます";
const LAUNCH_ONCE_LABEL: &str = "起動 (今回のみ)";
const CANCEL_LABEL: &str = "キャンセル";

const COLLABORATION_REASON: &str =
    "共同編集中のプロジェクトでは、サンドボックスの Claude Code は使えません。";
const REMOTE_REASON: &str =
    "リモートのプロジェクトでは、サンドボックスの Claude Code はまだ使えません。";
const UNSUPPORTED_OS_REASON: &str = "この OS では、サンドボックスの Claude Code はまだ使えません。";
const NO_FOLDER_REASON: &str =
    "フォルダーを開いていないため、サンドボックスの Claude Code は使えません。";

/// Starts the CLI outside the sandbox, once the user has confirmed it. A
/// parameter so that tests can observe the launch without a real terminal.
type HostLauncher =
    Rc<dyn Fn(&mut Workspace, Option<String>, &mut Window, &mut Context<Workspace>)>;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        register_launch_actions(
            workspace,
            Rc::new(terminal_view::session_tools::spawn_claude_code_on_host),
        );
    })
    .detach();
}

fn register_launch_actions(workspace: &mut Workspace, launch_on_host: HostLauncher) {
    workspace.register_action({
        let launch_on_host = launch_on_host.clone();
        move |workspace, action: &LaunchClaudeCode, window, cx| {
            let project = workspace.project().read(cx);
            let shape = ProjectShape::of(project, cx);
            let on_remote_host = project.is_via_remote_server();
            let effort = action.effort.clone();
            match route_for(shape) {
                #[cfg(target_os = "windows")]
                LaunchRoute::Sandbox => launch::launch_in_sandbox(workspace, effort, window, cx),
                #[cfg(not(target_os = "windows"))]
                LaunchRoute::Sandbox => confirm_and_launch_outside_sandbox(
                    effort,
                    Some(UNSUPPORTED_OS_REASON),
                    on_remote_host,
                    launch_on_host.clone(),
                    window,
                    cx,
                ),
                LaunchRoute::OutsideSandbox { reason } => confirm_and_launch_outside_sandbox(
                    effort,
                    Some(reason),
                    on_remote_host,
                    launch_on_host.clone(),
                    window,
                    cx,
                ),
            }
        }
    });
    workspace.register_action(
        move |workspace, action: &LaunchClaudeCodeOutsideSandbox, window, cx| {
            let on_remote_host = workspace.project().read(cx).is_via_remote_server();
            confirm_and_launch_outside_sandbox(
                action.effort.clone(),
                None,
                on_remote_host,
                launch_on_host.clone(),
                window,
                cx,
            );
        },
    );
}

/// What decides where the CLI can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProjectShape {
    /// Seen by collaborators: the sandbox's IDE session serves only a project
    /// nobody else sees.
    is_collaborative: bool,
    is_local: bool,
    has_folder: bool,
    sandbox_platform: bool,
}

impl ProjectShape {
    fn of(project: &Project, cx: &App) -> Self {
        Self {
            is_collaborative: project.is_via_collab() || project.remote_id().is_some(),
            is_local: project.is_local(),
            has_folder: !sandbox_roots(project, cx).is_empty(),
            sandbox_platform: cfg!(target_os = "windows"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchRoute {
    Sandbox,
    /// Always confirmed first, with `reason` saying why it is not the sandbox.
    OutsideSandbox {
        reason: &'static str,
    },
}

fn route_for(shape: ProjectShape) -> LaunchRoute {
    let reason = if shape.is_collaborative {
        COLLABORATION_REASON
    } else if !shape.is_local {
        REMOTE_REASON
    } else if !shape.sandbox_platform {
        UNSUPPORTED_OS_REASON
    } else if !shape.has_folder {
        NO_FOLDER_REASON
    } else {
        return LaunchRoute::Sandbox;
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
        OUTSIDE_SANDBOX_MESSAGE.to_string()
    } else {
        format!("`{program}` をサンドボックスの外で起動しますか？")
    }
}

/// On a remote project the CLI is the host's and runs with that user's
/// rights, neither this PC's nor Zed's.
fn outside_sandbox_detail(program: &str, on_remote_host: bool, reason: Option<&str>) -> String {
    let mut detail = if on_remote_host {
        format!(
            "接続先のホストの `{program}` を、そのホストのユーザーの権限で (サンドボックスの外で) \
             実行します。"
        )
    } else {
        format!(
            "{OUTSIDE_SANDBOX_NOTICE}。この PC の `{program}` をプロジェクトのフォルダーで起動します。"
        )
    };
    if let Some(reason) = reason {
        detail.push('\n');
        detail.push_str(reason);
    }
    detail
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
    let message = outside_sandbox_message(program);
    let detail = outside_sandbox_detail(program, on_remote_host, reason);
    let answer = window.prompt(
        PromptLevel::Warning,
        &message,
        Some(&detail),
        &[
            PromptButton::ok(LAUNCH_ONCE_LABEL),
            PromptButton::cancel(CANCEL_LABEL),
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

/// The platform-independent parts of a sandboxed session: where its files go,
/// what it runs, how its bridge reports, which leftovers are ours, and the IDE
/// glue — kept apart from the launch so that tests exercise them everywhere.
#[cfg(any(target_os = "windows", test))]
mod session {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;

    use anyhow::{Context as _, Result, ensure};
    use collections::HashMap;
    use futures::channel::mpsc;
    use futures::{AsyncRead, AsyncWrite, StreamExt as _};
    use gpui::{App, Entity, Global, SharedString, Task, WeakEntity};
    use project::agent_sandbox::is_project_key;
    pub(super) use project::agent_sandbox::project_key;
    use project::{AgentWriteScope, AgentWriterKind, Project, SandboxedWriterGuard};
    use terminal::Terminal;
    use workspace::Workspace;

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
    /// `<home>/.cache/zed-jp-enhance/ide/<session_id>/bridge.sock`.
    const RENDEZVOUS_HOME_RELATIVE: &str = ".cache/zed-jp-enhance/ide";
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
    const LAUNCH_SCRIPT: &str = r#"CLAUDE_CODE_TMPDIR=${TMPDIR:-/tmp}
USERPROFILE=$CLAUDE_CONFIG_DIR
export CLAUDE_CODE_TMPDIR USERPROFILE
exec "$0" "$@""#;
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
                "invalid project key {project_key:?}"
            );
            let unexpected = || format!("unexpected IDE rendezvous path {rendezvous:?}");
            let rendezvous_directory = rendezvous
                .strip_suffix(&format!("/{session_id}/{RENDEZVOUS_SOCKET_NAME}"))
                .with_context(unexpected)?;
            let home = rendezvous_directory
                .strip_suffix(&format!("/{RENDEZVOUS_HOME_RELATIVE}"))
                .filter(|home| home.starts_with('/'))
                .with_context(unexpected)?;
            let config_root = format!("{home}/{PROJECTS_HOME_RELATIVE}/{project_key}");
            let config_dir = format!("{config_root}/{CONFIG_DIR_NAME}");
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

    pub(super) fn session_environment(
        layout: &SessionLayout,
        inner_port: u16,
    ) -> Vec<(String, String)> {
        [
            ("CLAUDE_CONFIG_DIR", layout.config_dir.clone()),
            // On its own this makes the CLI connect to the IDE whose lock is
            // named after this port.
            ("CLAUDE_CODE_SSE_PORT", inner_port.to_string()),
            // Otherwise the CLI works the address out with `ip route` in WSL.
            ("CLAUDE_CODE_IDE_HOST_OVERRIDE", "127.0.0.1".to_string()),
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
            // `wsl.exe` does not carry the terminal's own over.
            ("TERM", "xterm-256color".to_string()),
            ("COLORTERM", "truecolor".to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect()
    }

    /// Everything the sandbox is asked for, before any of it is resolved.
    #[derive(Debug)]
    pub(super) struct SessionPlan {
        pub(super) command: sandbox::CommandAndArgs,
        /// The worktree roots (host paths), then the config root (in WSL).
        pub(super) writable_paths: Vec<PathBuf>,
        pub(super) allowed_hosts: Vec<String>,
        pub(super) ide_forward: sandbox::IdeForward,
    }

    pub(super) fn plan_session(
        layout: &SessionLayout,
        rendezvous: &str,
        claude_path: &str,
        effort: Option<&str>,
        roots: &[PathBuf],
        inner_port: u16,
    ) -> Result<SessionPlan> {
        ensure!(
            claude_path.starts_with('/') && !claude_path.contains(['\n', '\0']),
            "the Claude Code for the sandbox is not at an absolute path: {claude_path:?}"
        );
        let first_root = roots.first().context("no folder to run Claude Code in")?;
        let mut args = vec![
            "-c".to_string(),
            LAUNCH_SCRIPT.to_string(),
            claude_path.to_string(),
        ];
        if let Some(effort) = effort {
            args.extend(["--effort".to_string(), effort.to_string()]);
        }
        let mut writable_paths = roots.to_vec();
        writable_paths.push(PathBuf::from(&layout.config_root));
        Ok(SessionPlan {
            command: sandbox::CommandAndArgs {
                program: LAUNCH_SHELL.to_string(),
                args,
                env: session_environment(layout, inner_port)
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
                let state = if missing.iter().any(|missing| missing.as_ref() == *host) {
                    "未登録"
                } else {
                    "登録済み"
                };
                format!("{host} ({state})")
            })
            .collect::<Vec<_>>()
            .join("、");
        format!(
            "サンドボックスの Claude Code は Anthropic の次のホストにだけ接続します。\
             これらのホストは IDE 全体の許可リストに追加し、内蔵エージェントのコマンドにも\
             適用されます。\n許可が必要なホスト: {hosts}"
        )
    }

    /// The projects whose sandboxed session starts or runs, by
    /// [`session_key`], with the terminal once there is one.
    #[derive(Clone, Default)]
    pub(super) struct RunningSessions(Rc<RefCell<HashMap<String, Option<WeakEntity<Terminal>>>>>);

    impl Global for RunningSessions {}

    impl RunningSessions {
        /// The slot for `key`, or what already holds it: the session's
        /// terminal, `None` while that session still starts.
        pub(super) fn take(
            &self,
            key: String,
        ) -> std::result::Result<SessionSlot, Option<WeakEntity<Terminal>>> {
            {
                let mut sessions = self.0.borrow_mut();
                if let Some(existing) = sessions.get(&key) {
                    return Err(existing.clone());
                }
                sessions.insert(key.clone(), None);
            }
            Ok(SessionSlot {
                sessions: self.clone(),
                key,
            })
        }
    }

    /// A project's place among the running sessions. Dropping it frees it.
    pub(super) struct SessionSlot {
        sessions: RunningSessions,
        key: String,
    }

    impl SessionSlot {
        pub(super) fn set_terminal(&self, terminal: WeakEntity<Terminal>) {
            self.sessions
                .0
                .borrow_mut()
                .insert(self.key.clone(), Some(terminal));
        }
    }

    impl Drop for SessionSlot {
        fn drop(&mut self) {
            self.sessions.0.borrow_mut().remove(&self.key);
        }
    }

    /// One configuration, hence at most one session: the distro, by its
    /// registered name (WSL compares names without case), and the project's
    /// key.
    pub(super) fn session_key(distro: Option<&str>, project_key: &str) -> String {
        format!(
            "{}/{project_key}",
            distro.unwrap_or_default().to_lowercase()
        )
    }

    /// ADR-F50: what a session can write, spelled as worktrees are: its
    /// folders, and — through the distro's share, where the WSL home can be a
    /// worktree too — its configuration root, when `distro` names the distro.
    /// Shared with the external agents started in the WSL sandbox (第 2b 段).
    pub(super) fn session_write_scope(
        roots: &[PathBuf],
        config_root: &str,
        distro: Option<&str>,
    ) -> Vec<PathBuf> {
        project::agent_sandbox::wsl_session_write_scope(roots, config_root, distro)
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

    const SWEEP_DIRECTORY_PREFIX: &str = "zed-sweep-dir ";
    const SWEEP_LOCK_PREFIX: &str = "zed-sweep-lock ";
    const SWEEP_COMMAND_PREFIX: &str = "zed-sweep-cmd ";

    /// Leftovers of sessions whose bridge is gone (Zed or WSL went down
    /// before it could clean up).
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct StaleEntries {
        pub(super) rendezvous: Vec<String>,
        pub(super) locks: Vec<String>,
    }

    impl StaleEntries {
        pub(super) fn is_empty(&self) -> bool {
            self.rendezvous.is_empty() && self.locks.is_empty()
        }
    }

    /// From the sweep's listing (see `launch::SWEEP_LIST_SCRIPT`): the entries
    /// named the way this launcher names them that no running process — a
    /// bridge, or the helper connecting to it — names on its command line.
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

    fn is_lock_file_name(name: &str) -> bool {
        name.strip_suffix(".lock").is_some_and(|port| {
            (1..=5).contains(&port.len())
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok()
        })
    }

    const HELPER_FAILURE_MARKER: &str = "zed-wsl-sandbox: ";

    /// Why the session never started, when the helper refused to build the
    /// sandbox: it exits 126 after a marker line, which lands on the session's
    /// terminal screen rather than in anything Zed reads.
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

    /// The IDE session, started before the bridge: its auth token goes into
    /// the lock file the bridge writes before any connection can arrive.
    pub(super) struct PendingIdeSession {
        session: ide_server::SandboxIdeSessionHandle,
        connections: mpsc::UnboundedSender<sandbox::BridgeConnection>,
    }

    /// Keeps the IDE side of a session alive. Dropping it ends the bridge's
    /// stdin, so the bridge removes its socket and lock and exits, and ends
    /// the IDE session, which closes its diff tabs.
    pub(super) struct IdeLink {
        _mux: sandbox::BridgeMuxHandle,
        _forward: Task<()>,
        _session: ide_server::SandboxIdeSessionHandle,
    }

    impl PendingIdeSession {
        pub(super) fn start(
            workspace: WeakEntity<Workspace>,
            mapper: Arc<dyn ide_server::PathMapper>,
            roots: Vec<PathBuf>,
            cx: &mut App,
        ) -> (Self, String) {
            let (sender, receiver) = mpsc::unbounded();
            let (session, info) =
                ide_server::start_sandbox_session(workspace, receiver, mapper, roots, cx);
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
        pub(super) fn attach<R, W>(self, reader: R, writer: W, cx: &mut App) -> IdeLink
        where
            R: AsyncRead + Unpin + Send + 'static,
            W: AsyncWrite + Unpin + Send + 'static,
        {
            let executor = cx.background_executor().clone();
            let (mut connections, mux) =
                sandbox::spawn_bridge_mux(reader, writer, |future| executor.spawn(future).detach());
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

/// The sandboxed launch itself, which needs WSL.
#[cfg(target_os = "windows")]
mod launch {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context as _, Result, anyhow, bail};
    use fs::Fs;
    use gpui::{
        App, AsyncWindowContext, BackgroundExecutor, Context, Entity, FutureExt as _, PromptButton,
        PromptLevel, SharedString, Task, WeakEntity, Window,
    };
    use project::SandboxedWriterGuard;
    use project::agent_sandbox::canonical_project_key_roots;
    use rand::Rng as _;
    use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
    use terminal::Terminal;
    use terminal_view::TerminalView;
    use terminal_view::terminal_panel::TerminalPanel;
    use util::ResultExt as _;
    use util::command::Stdio;
    use workspace::{Toast, Workspace, notifications::NotificationId};

    use super::session::{
        CLAUDE_HOSTS, IdeLink, PendingIdeSession, RunningSessions, SessionLayout, SessionPlan,
        SessionSlot, helper_failure_message, hosts_detail, new_session_id, plan_session,
        project_key, register_session_writer, session_key, session_write_scope, stale_entries,
    };
    use crate::agent_registry_ui::{add_consented_hosts, allowlist_write_error, missing_hosts};

    const LAUNCH_FAILED_PREFIX: &str = "Claude Code (サンドボックス) を起動できませんでした: ";
    const PREPARING_MESSAGE: &str = "Claude Code (サンドボックス) を準備しています…";
    const PREPARING_SANDBOX_MESSAGE: &str = "サンドボックスと IDE 連携を準備しています…";
    const SANDBOX_TAB_LABEL: &str = "Claude Code (サンドボックス)";
    const HOSTS_MESSAGE: &str = "Claude Code (サンドボックス) の通信に許可リストへの追加が必要です";
    const ADD_HOSTS_LABEL: &str = "許可リストに追加して起動";
    const DECLINE_HOSTS_LABEL: &str = "追加せず中止";

    /// One `wsl.exe` round trip, which a cold WSL can take this long to answer.
    const WSL_STEP_TIMEOUT: Duration = Duration::from_secs(90);
    const BRIDGE_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    /// Where the CLI's IDE port is picked, inside the session's own network
    /// namespace. Random, so that a lock a crashed session left behind is
    /// unlikely to be in the way before the sweep removes it.
    const INNER_PORTS: std::ops::Range<u16> = 20000..60000;

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

    struct SandboxLaunchProgress;

    fn progress_id() -> NotificationId {
        NotificationId::unique::<SandboxLaunchProgress>()
    }

    fn show_progress(workspace: &mut Workspace, message: &str, cx: &mut Context<Workspace>) {
        workspace.show_toast(Toast::new(progress_id(), message.to_string()), cx);
    }

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
                workspace.show_error(format!("{LAUNCH_FAILED_PREFIX}{error:#}"), cx);
                return;
            }
        };
        let fs = workspace.app_state().fs.clone();
        show_progress(workspace, PREPARING_MESSAGE, cx);
        cx.spawn_in(window, async move |workspace, cx| {
            let outcome = start_session(
                workspace.clone(),
                fs,
                roots,
                key_roots,
                git_dirs,
                distro,
                effort,
                cx,
            )
            .await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.dismiss_toast(&progress_id(), cx);
                    match outcome {
                        Ok(()) => {}
                        Err(error) if error.is::<sandbox::IdeBridgeSessionRunning>() => {
                            show_already_running(workspace, cx)
                        }
                        Err(error) => {
                            workspace.show_error(format!("{LAUNCH_FAILED_PREFIX}{error:#}"), cx)
                        }
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
            bail!("no folder to run Claude Code in");
        }
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
        // The key's folders are the project's, not the session's writable
        // ones: a single file's folder names the project without the session
        // being allowed to write it.
        let canonical_roots = canonical_project_key_roots(&fs, key_roots).await;
        let project_key = project_key(&canonical_roots);
        // The default distro by its name, so that it and a path naming it share
        // one slot, as they share one configuration. Without the registry's
        // word, the name the project's paths give, if any.
        let distro_name = sandbox::registered_wsl_distro_name(distro.as_deref()).or_else(|| {
            log::warn!("[claude-sandbox] the registry does not name WSL distro {distro:?}");
            distro.clone()
        });
        let key = session_key(distro_name.as_deref(), &project_key);
        // Held until the session ends: a second one on the same configuration
        // would share its lock directory and its login.
        let Some(slot) = cx.update(|window, cx| take_session_slot(key, &workspace, window, cx))?
        else {
            return Ok(());
        };
        let HostsConsent::Granted { network_ceiling } = consent_to_claude_hosts(cx).await? else {
            return Ok(());
        };
        let claude_path = claude_path(workspace.clone(), distro.clone(), cx).await?;
        workspace.update(cx, |workspace, cx| {
            show_progress(workspace, PREPARING_SANDBOX_MESSAGE, cx)
        })?;

        let executor = cx.background_executor().clone();
        let session_id = new_session_id();
        let inner_port = rand::rng().random_range(INNER_PORTS);
        let rendezvous = in_wsl(&executor, {
            let distro = distro.clone();
            let session_id = session_id.clone();
            async move { sandbox::ide_rendezvous_path(distro.as_deref(), &session_id).await }
        })
        .await?;
        let layout =
            SessionLayout::from_rendezvous(&rendezvous, &session_id, &project_key, inner_port)?;
        let plan = plan_session(
            &layout,
            &rendezvous,
            &claude_path,
            effort.as_deref(),
            &roots,
            inner_port,
        )?;

        let mapper: Arc<dyn ide_server::PathMapper> = Arc::new(ide_server::WslPathMapper {
            // The default distro's name is not needed: its paths reach the IDE
            // session only as `/mnt/<drive>/…`, which the mapper takes alone.
            distro: distro.clone().unwrap_or_default(),
        });
        let (pending, auth_token) = cx.update(|_, cx| {
            PendingIdeSession::start(workspace.clone(), mapper.clone(), roots.clone(), cx)
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
            // Held by the bridge while it runs: a session another Zed
            // started on this configuration turns this one away.
            let config_root = layout.config_root.clone();
            async move {
                sandbox::ide_bridge_invocation(
                    distro.as_deref(),
                    &rendezvous,
                    Some((&lock_file, &lock_content)),
                    Some(&config_root),
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
            slot,
        };
        worktree_writer_resolved.await;
        let (wrapped, sandbox) =
            in_wsl(&executor, wrap_session(plan, git_dirs, network_ceiling)).await?;
        resources.sandbox = Some(sandbox);
        let terminal = spawn_terminal(&workspace, wrapped, &session_id, cx).await?;
        resources.slot.set_terminal(terminal.downgrade());
        watch_session(terminal, resources, workspace, cx)
    }

    const ALREADY_RUNNING_MESSAGE: &str =
        "このプロジェクトの Claude Code (サンドボックス) はすでに実行中です";

    struct AlreadyRunning;

    pub(super) fn already_running_id() -> NotificationId {
        NotificationId::unique::<AlreadyRunning>()
    }

    fn show_already_running(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
        workspace.show_toast(
            Toast::new(already_running_id(), ALREADY_RUNNING_MESSAGE.to_string()).autohide(),
            cx,
        );
    }

    /// The project's slot, or `None` after saying that its session already
    /// runs and showing that session's tab.
    pub(super) fn take_session_slot(
        key: String,
        workspace: &WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<SessionSlot> {
        let running = cx.default_global::<RunningSessions>().clone();
        let existing = match running.take(key) {
            Ok(slot) => return Some(slot),
            Err(existing) => existing,
        };
        workspace
            .update(cx, |workspace, cx| {
                show_already_running(workspace, cx);
                if let Some(terminal) = existing.and_then(|terminal| terminal.upgrade()) {
                    reveal_terminal(workspace, &terminal, window, cx);
                }
            })
            .log_err();
        None
    }

    /// Brings the tab of `terminal` forward, when it is in this workspace's
    /// terminal panel.
    fn reveal_terminal(
        workspace: &mut Workspace,
        terminal: &Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
            return;
        };
        let found = terminal_panel
            .read(cx)
            .panes()
            .into_iter()
            .find_map(|pane| {
                let index = pane.read(cx).items().position(|item| {
                    item.downcast::<TerminalView>()
                        .is_some_and(|view| view.read(cx).terminal() == terminal)
                })?;
                Some((pane.clone(), index))
            });
        let Some((pane, index)) = found else {
            return;
        };
        workspace.focus_panel::<TerminalPanel>(window, cx);
        pane.update(cx, |pane, cx| {
            pane.activate_item(index, true, true, window, cx)
        });
    }

    enum HostsConsent {
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
    async fn consent_to_claude_hosts(cx: &mut AsyncWindowContext) -> Result<HostsConsent> {
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
            HOSTS_MESSAGE,
            Some(&detail),
            &[
                PromptButton::ok(ADD_HOSTS_LABEL),
                PromptButton::cancel(DECLINE_HOSTS_LABEL),
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
    fn editor_allowlist(cx: &App) -> Option<Vec<String>> {
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
                    distro,
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
        Ok(binary.path_in_wsl)
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
                anyhow!(
                    "WSL が {} 秒以内に応答しませんでした。しばらく待ってから再試行し、\
                     改善しない場合は `wsl --shutdown` で WSL を再起動してください。",
                    WSL_STEP_TIMEOUT.as_secs()
                )
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
                    format!(
                        "書込を許可するフォルダー `{}` を確認できませんでした",
                        path.display()
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

    async fn spawn_terminal(
        workspace: &WeakEntity<Workspace>,
        wrapped: sandbox::WrappedCommand,
        session_id: &str,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<Terminal>> {
        let spawn_task = SpawnInTerminal {
            id: TaskId(format!("terminal::claude-code-sandbox::{session_id}")),
            full_label: SANDBOX_TAB_LABEL.to_string(),
            label: SANDBOX_TAB_LABEL.to_string(),
            command: Some(wrapped.program),
            args: wrapped.args,
            command_label: SANDBOX_TAB_LABEL.to_string(),
            env: wrapped.env.into_iter().collect(),
            use_new_terminal: true,
            allow_concurrent_runs: true,
            reveal: RevealStrategy::Always,
            reveal_target: RevealTarget::Dock,
            show_summary: false,
            show_command: false,
            // The same `wsl.exe` line run again would find its bridge gone.
            show_rerun: false,
            ..SpawnInTerminal::default()
        };
        let terminal_panel = workspace
            .read_with(cx, |workspace, cx| workspace.panel::<TerminalPanel>(cx))?
            .context("ターミナルパネルが利用できません。")?;
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
        terminal
            .upgrade()
            .context("the session's terminal closed as soon as it opened")
    }

    /// What a running session holds on to.
    struct SessionResources {
        ide: IdeLink,
        bridge: util::command::Child,
        _bridge_log: Task<()>,
        sandbox: Option<sandbox::Sandbox>,
        /// ADR-F50: the session's hold on the worktrees it can write.
        worktree_writer: SandboxedWriterGuard,
        /// Freed last, once the bridge is gone.
        slot: SessionSlot,
    }

    impl SessionResources {
        async fn shut_down(self, executor: &BackgroundExecutor) {
            let SessionResources {
                ide,
                mut bridge,
                _bridge_log,
                sandbox,
                worktree_writer,
                slot,
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
            drop(slot);
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
            if let Some(workspace) = workspace.upgrade() {
                workspace.update(cx, |workspace, cx| {
                    // Saying that the session runs is wrong once it has ended.
                    workspace.dismiss_toast(&already_running_id(), cx);
                    if let Some(message) = message {
                        workspace.show_error(message, cx);
                    }
                });
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
    use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
    use project::worktree_code_execution::{ResumeOutcome, WorktreeCodeState};
    use project::{FakeFs, Project, WorktreeCodeExecution, WorktreeId};
    use serde_json::{Value, json};
    use util::path;
    use workspace::{MultiWorkspace, Workspace};
    use zed_actions::terminal::{LaunchClaudeCode, LaunchClaudeCodeOutsideSandbox};

    use super::session::{
        PendingIdeSession, RunningSessions, SessionLayout, StaleEntries, helper_failure_message,
        hosts_detail, plan_session, project_key, register_session_writer, session_key,
        session_write_scope, stale_entries,
    };
    use super::{
        CLAUDE_PROGRAM, COLLABORATION_REASON, HostLauncher, LaunchRoute, NO_FOLDER_REASON,
        OUTSIDE_SANDBOX_MESSAGE, OUTSIDE_SANDBOX_NOTICE, ProjectShape, REMOTE_REASON,
        UNSUPPORTED_OS_REASON, outside_sandbox_detail, outside_sandbox_message,
        register_launch_actions, route_for,
    };

    const SESSION_ID: &str = "claude-0123456789abcdef0123456789abcdef";
    const RENDEZVOUS: &str =
        "/root/.cache/zed-jp-enhance/ide/claude-0123456789abcdef0123456789abcdef/bridge.sock";
    const PORT: u16 = 45123;
    const CLAUDE_PATH: &str = "/root/.cache/zed-jp-enhance/agents/3a62/claude";
    const PROJECT_KEY: &str = "0123456789abcdef";
    const CONFIG_ROOT: &str = "/root/.zed-jp-enhance-claude/projects/0123456789abcdef";

    fn layout() -> SessionLayout {
        SessionLayout::from_rendezvous(RENDEZVOUS, SESSION_ID, PROJECT_KEY, PORT)
            .expect("the shape `sandbox::ide_rendezvous_path` documents")
    }

    #[test]
    fn launches_in_the_sandbox_only_where_it_can() {
        let sandboxable = ProjectShape {
            is_collaborative: false,
            is_local: true,
            has_folder: true,
            sandbox_platform: true,
        };
        assert_eq!(route_for(sandboxable), LaunchRoute::Sandbox);
        for (shape, reason) in [
            (
                ProjectShape {
                    is_local: false,
                    ..sandboxable
                },
                REMOTE_REASON,
            ),
            (
                ProjectShape {
                    is_collaborative: true,
                    ..sandboxable
                },
                COLLABORATION_REASON,
            ),
            (
                ProjectShape {
                    is_collaborative: true,
                    is_local: false,
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
                    is_local: false,
                    sandbox_platform: false,
                    ..sandboxable
                },
                REMOTE_REASON,
            ),
            (
                ProjectShape {
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

    #[test]
    fn the_session_files_follow_the_rendezvous() {
        assert_eq!(
            layout(),
            SessionLayout {
                rendezvous_directory: "/root/.cache/zed-jp-enhance/ide".to_string(),
                config_root: CONFIG_ROOT.to_string(),
                config_dir: format!("{CONFIG_ROOT}/config"),
                lock_directory: format!("{CONFIG_ROOT}/config/ide"),
                lock_file: format!("{CONFIG_ROOT}/config/ide/45123.lock"),
            }
        );
        for unexpected in [
            "/root/.cache/zed-jp-enhance/ide/claude-other/bridge.sock".to_string(),
            format!("/root/elsewhere/{SESSION_ID}/bridge.sock"),
            format!("/.cache/zed-jp-enhance/ide/{SESSION_ID}/bridge.sock"),
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

    #[test]
    fn the_outside_sandbox_confirmation_names_what_runs_where() {
        let local = outside_sandbox_detail(CLAUDE_PROGRAM, false, Some(NO_FOLDER_REASON));
        assert!(local.starts_with(OUTSIDE_SANDBOX_NOTICE), "{local}");
        assert!(local.contains("この PC の `claude`"), "{local}");
        assert!(local.ends_with(NO_FOLDER_REASON), "{local}");

        let remote = outside_sandbox_detail(CLAUDE_PROGRAM, true, Some(REMOTE_REASON));
        assert!(
            remote.starts_with("接続先のホストの `claude` を"),
            "{remote}"
        );
        assert!(
            !remote.contains("この PC") && !remote.contains("Zed 本体の権限"),
            "{remote}"
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
            let detail = outside_sandbox_detail("codex", on_remote_host, None);
            assert!(
                detail.contains("`codex`") && !detail.contains("claude"),
                "{detail}"
            );
        }
    }

    #[test]
    fn the_hosts_consent_says_they_apply_to_the_whole_ide() {
        let detail = hosts_detail(&["api.anthropic.com".into()]);
        assert!(
            detail.contains(
                "IDE 全体の許可リストに追加し、内蔵エージェントのコマンドにも適用されます"
            ),
            "{detail}"
        );
        assert!(
            detail.ends_with(
                "許可が必要なホスト: api.anthropic.com (未登録)、platform.claude.com (登録済み)"
            ),
            "{detail}"
        );
    }

    /// A second session on the same configuration is refused until the
    /// first one ends; another project is not held up.
    #[test]
    fn one_session_per_project_configuration() {
        let running = RunningSessions::default();
        let key = session_key(Some("Ubuntu"), PROJECT_KEY);
        let slot = running.take(key.clone()).expect("the first session starts");
        assert!(
            matches!(running.take(key.clone()), Err(None)),
            "a session still starting holds the slot"
        );
        assert!(
            matches!(
                running.take(session_key(Some("ubuntu"), PROJECT_KEY)),
                Err(None)
            ),
            "the distro's name in another case is the same distro"
        );
        assert!(
            running
                .take(session_key(Some("Debian"), PROJECT_KEY))
                .is_ok(),
            "another distro's configuration is another"
        );
        assert!(
            running
                .take(session_key(Some("Ubuntu"), "fedcba9876543210"))
                .is_ok()
        );
        drop(slot);
        assert!(running.take(key).is_ok(), "an ended session frees it");
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
                    "/root/.zed-jp-enhance-claude/projects/0123456789abcdef/config"
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
        )
        .expect("a valid plan");
        assert_eq!(
            without_effort.command.args.get(2..),
            Some(&[CLAUDE_PATH.to_string()][..])
        );
        assert!(
            plan_session(&layout, RENDEZVOUS, "claude", None, &[root], PORT).is_err(),
            "the CLI is never looked up on PATH"
        );
        assert!(plan_session(&layout, RENDEZVOUS, CLAUDE_PATH, None, &[], PORT).is_err());
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
        assert!(message.contains("IDE 連携"), "{message}");
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
                PathBuf::from(
                    r"\\wsl.localhost\Ubuntu\root\.zed-jp-enhance-claude\projects\0123456789abcdef"
                ),
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
        cx.simulate_prompt_answer("キャンセル");
        cx.run_until_parked();
        assert!(launches.borrow().is_empty(), "declining starts nothing");

        for launch in 1..=2 {
            cx.dispatch_action(LaunchClaudeCodeOutsideSandbox {
                effort: Some("high".to_string()),
            });
            cx.run_until_parked();
            assert!(cx.has_pending_prompt(), "launch {launch} asks again");
            cx.simulate_prompt_answer("起動 (今回のみ)");
            cx.run_until_parked();
            assert_eq!(launches.borrow().len(), launch);
        }
        assert_eq!(launches.borrow().last(), Some(&Some("high".to_string())));
    }

    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn saying_that_the_session_already_runs_goes_away_by_itself(cx: &mut TestAppContext) {
        use super::launch::{already_running_id, take_session_slot};

        let (_project, workspace, mut cx) = open_workspace(&[path!("/test").as_ref()], cx).await;
        let weak_workspace = workspace.downgrade();
        let key = "ubuntu/0123456789abcdef".to_string();
        let slot =
            cx.update(|window, cx| take_session_slot(key.clone(), &weak_workspace, window, cx));
        assert!(slot.is_some(), "the first launch takes the project's slot");
        let second = cx.update(|window, cx| take_session_slot(key, &weak_workspace, window, cx));
        assert!(second.is_none(), "the second launch finds it taken");
        let says_already_running = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                workspace.notification_ids().contains(&already_running_id())
            })
        };
        assert!(says_already_running(&mut cx));

        cx.executor()
            .advance_clock(std::time::Duration::from_secs(6));
        cx.run_until_parked();
        assert!(
            !says_already_running(&mut cx),
            "the notice hides itself after a few seconds"
        );
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
        cx.simulate_prompt_answer("起動 (今回のみ)");
        cx.run_until_parked();
        assert_eq!(*launches.borrow(), [None]);
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
        assert_eq!(response["result"]["serverInfo"]["name"], json!("Zed"));
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
