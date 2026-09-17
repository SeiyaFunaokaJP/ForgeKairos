//! Claude Code CLI の起動導線 (ADR-F49 決定 1・3・6、第 3 段)。
//!
//! 「Claude Code を起動」は、
//! * Windows のローカルのプロジェクトでは、サンドボックス (WSL のヘルパー) の対話セッションとして
//!   CLI を起動し、IDE 連携はヘルパーの IDE 転送 → ブリッジ → サンドボックス用の IDE セッションで繋ぐ。
//! * SSH で繋いだ Linux のプロジェクトでは、接続先の remote_server をサンドボックスヘルパーとして
//!   同じ形で起動する (ADR-F18)。CLI は**この PC で取得・検証**してから接続先へ送る。IDE 連携は、
//!   ホスト側のファイルの実体を見る確認 (`ide_server` の `RealRoots`) がクライアントの
//!   ファイルシステムしか見られないため、この段では繋がない。
//! * それ以外 (共同編集・対応しない接続方法・Windows 以外のローカル・フォルダーなし) と
//!   「サンドボックスの外で起動」は、その場の `claude` を起動のたびに確認してから起動する。
//!
//! ハンドラーが `terminal_view` ではなくここにあるのは、IDE セッション (`ide_server`) と Linux 版の
//! CLI の取得 (F49-L1) を使うため。`terminal_view` はどちらにも依存できない (`agent_ui` が
//! `terminal_view` に依存している)。

use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, PromptButton, PromptLevel,
    WeakEntity, Window,
};
use project::Project;
use task::{Shell, SpawnInTerminal};
use terminal::Terminal;
use terminal_view::TerminalView;
use terminal_view::terminal_panel::prepare_task_for_spawn;
use util::ResultExt as _;
use workspace::Workspace;
use zed_actions::terminal::{LaunchClaudeCode, LaunchClaudeCodeOutsideSandbox};

use crate::AgentPanel;

/// The program the launch actions start.
const CLAUDE_PROGRAM: &str = "claude";
const OUTSIDE_SANDBOX_MESSAGE: &str = "Claude Code をサンドボックスの外で起動しますか？";
/// ADR-F46 A-3 の帯と同じ言い回し。
const OUTSIDE_SANDBOX_NOTICE: &str = "サンドボックスの外 (Zed 本体の権限) で実行されます";
const LAUNCH_ONCE_LABEL: &str = "起動 (今回のみ)";
const CANCEL_LABEL: &str = "キャンセル";

const COLLABORATION_REASON: &str =
    "共同編集中のプロジェクトでは、サンドボックスの Claude Code は使えません。";
/// SSH 以外の接続 (WSL リモート接続・Docker など)。置き場も待ち合わせもそれぞれ違うので、
/// この段では外へ回す。
const REMOTE_CONNECTION_REASON: &str =
    "この接続方法では、サンドボックスの Claude Code はまだ使えません。";
const REMOTE_NOT_LINUX_REASON: &str =
    "接続先が Linux ではないため、サンドボックスの Claude Code は使えません。";
const UNSUPPORTED_OS_REASON: &str = "この OS では、サンドボックスの Claude Code はまだ使えません。";
const NO_FOLDER_REASON: &str =
    "フォルダーを開いていないため、サンドボックスの Claude Code は使えません。";

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
            let project = workspace.project().read(cx);
            let shape = ProjectShape::of(project, cx);
            let on_remote_host = project.is_via_remote_server();
            let effort = action.effort.clone();
            match route_for(shape) {
                #[cfg(target_os = "windows")]
                LaunchRoute::LocalSandbox => {
                    launch::launch_in_sandbox(workspace, effort, window, cx)
                }
                #[cfg(not(target_os = "windows"))]
                LaunchRoute::LocalSandbox => confirm_and_launch_outside_sandbox(
                    effort,
                    Some(UNSUPPORTED_OS_REASON),
                    on_remote_host,
                    launch_on_host.clone(),
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
    /// `None` for a local project; otherwise how it is connected.
    remote: Option<RemoteShape>,
    has_folder: bool,
    sandbox_platform: bool,
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
        }
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
}

fn route_for(shape: ProjectShape) -> LaunchRoute {
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

/// CLI のタブを持っているペインの持ち主。前面化のしかたが持ち主ごとに違う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TerminalTabHost {
    AgentPanel,
    TerminalPanel,
    /// 中央のペイン、またはほかのドックのパネル (タブは動かせる)。
    Elsewhere,
}

/// このワークスペースで `terminal` を出しているタブ (持ち主・ペイン・並び順)。
///
/// ADR-F53 でセッションの端末はエージェントパネルのタブに置かれるようになり、
/// タブはそこから中央や別のドックへ動かせるので、「二重起動です」と言うときの
/// 前面化はどこにあっても効く必要がある。エージェントパネル → ターミナルパネル
/// → それ以外の順に探すのは、前 2 つは畳まれていることがあり、先に開かないと
/// タブを選んでも何も見えないから。
pub(crate) fn terminal_tab_location(
    workspace: &Workspace,
    terminal: &Entity<Terminal>,
    cx: &App,
) -> Option<(TerminalTabHost, Entity<workspace::Pane>, usize)> {
    let holds_terminal = |pane: &Entity<workspace::Pane>| {
        pane.read(cx).items().position(|item| {
            item.downcast::<TerminalView>()
                .is_some_and(|view| view.read(cx).terminal() == terminal)
        })
    };
    let in_panes = |panes: Vec<Entity<workspace::Pane>>| {
        panes
            .into_iter()
            .find_map(|pane| holds_terminal(&pane).map(|index| (pane, index)))
    };

    if let Some((pane, index)) = workspace
        .panel::<AgentPanel>(cx)
        .and_then(|panel| in_panes(workspace::Panel::panes(panel.read(cx))))
    {
        return Some((TerminalTabHost::AgentPanel, pane, index));
    }
    if let Some((pane, index)) = workspace
        .panel::<terminal_view::terminal_panel::TerminalPanel>(cx)
        .and_then(|panel| in_panes(panel.read(cx).panes().into_iter().cloned().collect()))
    {
        return Some((TerminalTabHost::TerminalPanel, pane, index));
    }
    let (pane, index) = in_panes(workspace.all_panes(cx))?;
    Some((TerminalTabHost::Elsewhere, pane, index))
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
    ),
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(panel) = agent_panel_for_cli_tab(workspace, cx) else {
        without_agent_panel(workspace, effort, window, cx);
        return;
    };

    let spawn_task = terminal_view::session_tools::claude_code_launch_task(effort, workspace, cx);
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

        if let Err(error) = open_terminal_tab_in_agent_panel(
            &workspace,
            panel,
            spawn_task,
            SessionTerminalKind::Host,
            cx,
        )
        .await
        {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(
                        format!("ターミナルでコマンドを実行できませんでした: {error:#}"),
                        cx,
                    )
                })
                .log_err();
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
        Task, WeakEntity, Window,
    };
    use project::agent_sandbox::is_project_key;
    pub(super) use project::agent_sandbox::project_key;
    use project::{AgentWriteScope, AgentWriterKind, Project, SandboxedWriterGuard};
    use task::{RevealStrategy, RevealTarget, SpawnInTerminal, TaskId};
    use terminal::Terminal;
    use terminal_view::terminal_panel::TerminalPanel;
    use util::ResultExt as _;
    use workspace::{Toast, Workspace, notifications::NotificationId};

    use crate::AgentPanel;
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
    #[cfg(any(target_os = "windows", test))]
    const LOCK_DIRECTORY_NAME: &str = "ide";
    /// The shape `sandbox::ide_rendezvous_path` documents:
    /// `<home>/.cache/zed-jp-enhance/ide/<session_id>/bridge.sock`.
    #[cfg(any(target_os = "windows", test))]
    const RENDEZVOUS_HOME_RELATIVE: &str = ".cache/zed-jp-enhance/ide";
    #[cfg(any(target_os = "windows", test))]
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
    #[cfg(any(target_os = "windows", test))]
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

    #[cfg(any(target_os = "windows", test))]
    impl SessionLayout {
        pub(super) fn from_rendezvous(
            rendezvous: &str,
            session_id: &str,
            project_key: &str,
            inner_port: u16,
        ) -> Result<Self> {
            ensure!(
                is_project_key(project_key),
                "プロジェクトの識別子が不正です (内部エラー): {project_key:?}"
            );
            let unexpected =
                || format!("IDE の待ち合わせパスの形が想定と違います (内部エラー): {rendezvous:?}");
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
            "プロジェクトの識別子が不正です (内部エラー): {project_key:?}"
        );
        ensure!(
            home.starts_with('/') && !home.ends_with('/') && !home.contains(['\n', '\0']),
            "接続先のホームディレクトリの形が想定と違います (内部エラー): {home:?}"
        );
        let config_root = format!("{home}/{PROJECTS_HOME_RELATIVE}/{project_key}");
        let config_dir = format!("{config_root}/{CONFIG_DIR_NAME}");
        Ok((config_root, config_dir))
    }

    /// What the CLI is started with. `ide_port` is the port inside the sandbox
    /// the IDE channel listens on, and `None` for a session without one (the
    /// SSH host in 第 3 段): then nothing points the CLI at an IDE, so it runs
    /// as it would in a plain terminal instead of waiting for one.
    pub(super) fn session_environment(
        config_dir: &str,
        ide_port: Option<u16>,
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
    pub(super) fn launch_invocation(
        claude_path: &str,
        effort: Option<&str>,
    ) -> Result<(String, Vec<String>)> {
        ensure!(
            claude_path.starts_with('/') && !claude_path.contains(['\n', '\0']),
            "サンドボックスで使う Claude Code の場所が絶対パスではありません (内部エラー): \
             {claude_path:?}"
        );
        let mut args = vec![
            "-c".to_string(),
            LAUNCH_SCRIPT.to_string(),
            claude_path.to_string(),
        ];
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
    ) -> Result<SessionPlan> {
        let first_root = roots
            .first()
            .context("Claude Code を実行するフォルダーがありません。")?;
        let (program, args) = launch_invocation(claude_path, effort)?;
        let mut writable_paths = roots.to_vec();
        writable_paths.push(PathBuf::from(&layout.config_root));
        Ok(SessionPlan {
            command: sandbox::CommandAndArgs {
                program,
                args,
                env: session_environment(&layout.config_dir, Some(inner_port))
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

    /// One configuration, hence at most one session: where it runs — the
    /// distro by its registered name (WSL compares names without case), or the
    /// host by its connection — and the project's key.
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

    #[cfg(any(target_os = "windows", test))]
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
        "Claude Code (サンドボックス) を起動できませんでした: ";
    pub(super) const PREPARING_MESSAGE: &str = "Claude Code (サンドボックス) を準備しています…";
    pub(super) const PREPARING_SANDBOX_MESSAGE: &str = "サンドボックスと IDE 連携を準備しています…";
    pub(super) const SANDBOX_TAB_LABEL: &str = "Claude Code (サンドボックス)";
    const HOSTS_MESSAGE: &str = "Claude Code (サンドボックス) の通信に許可リストへの追加が必要です";
    const ADD_HOSTS_LABEL: &str = "許可リストに追加して起動";
    const DECLINE_HOSTS_LABEL: &str = "追加せず中止";

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

    pub(super) const ALREADY_RUNNING_MESSAGE: &str =
        "このプロジェクトの Claude Code (サンドボックス) はすでに実行中です";

    struct AlreadyRunning;

    pub(super) fn already_running_id() -> NotificationId {
        NotificationId::unique::<AlreadyRunning>()
    }

    pub(super) fn show_already_running(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
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

    /// Brings the tab of `terminal` forward, wherever it is in this workspace:
    /// the agent panel (ADR-F53), the terminal panel, the center, or another
    /// dock. The panel that holds it is opened first, since activating a tab in
    /// a hidden dock would leave the user looking at nothing.
    pub(super) fn reveal_terminal(
        workspace: &mut Workspace,
        terminal: &Entity<Terminal>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some((host, pane, index)) = super::terminal_tab_location(workspace, terminal, cx)
        else {
            return;
        };
        match host {
            super::TerminalTabHost::AgentPanel => {
                workspace.focus_panel::<AgentPanel>(window, cx);
            }
            super::TerminalTabHost::TerminalPanel => {
                workspace.focus_panel::<TerminalPanel>(window, cx);
            }
            // 中央やほかのドックへ移されたタブ。`activate_item` の
            // `activate_pane` がそのペインを前に出す。
            super::TerminalTabHost::Elsewhere => {}
        }
        pane.update(cx, |pane, cx| {
            pane.activate_item(index, true, true, window, cx)
        });
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
            id: TaskId(format!("terminal::claude-code-sandbox::{session_id}")),
            full_label: SANDBOX_TAB_LABEL.to_string(),
            label: SANDBOX_TAB_LABEL.to_string(),
            command: Some(program),
            args,
            command_label: SANDBOX_TAB_LABEL.to_string(),
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
            .context("セッションのターミナルが開いた直後に閉じられました。")
    }

    /// The IDE channel, which only the WSL sandbox has: on an SSH host the
    /// session's file requests would be checked against this machine's file
    /// system (`ide_server::path_mapping`), which knows nothing about the
    /// host's, so 第 3 段 runs without one.
    #[cfg(any(target_os = "windows", test))]
    pub(super) use ide_link::{IdeLink, PendingIdeSession};

    #[cfg(any(target_os = "windows", test))]
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
            pub(crate) fn start(
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
/// It runs with no IDE channel: the requests a sandboxed session makes about
/// files are checked against a file system `ide_server` can open
/// (`ide_server::path_mapping`), and on a remote project that is this machine's,
/// not the host's — every request would be refused as outside the workspace.
/// Nothing points the CLI at an IDE, so it behaves as it does in a plain
/// terminal; wiring it is what a later step has to do.
mod remote_launch {
    use std::path::PathBuf;

    use anyhow::{Context as _, Result, anyhow, bail};
    use collections::HashMap;
    use gpui::{AsyncWindowContext, Context, Entity, WeakEntity, Window};
    use project::{Project, SandboxedWriterGuard};
    use remote::RemoteClient;
    use terminal::Terminal;
    use util::ResultExt as _;
    use workspace::Workspace;

    use super::session::{
        CLAUDE_HOSTS, HostsConsent, LAUNCH_FAILED_PREFIX, PREPARING_MESSAGE,
        PREPARING_SANDBOX_MESSAGE, SessionSlot, already_running_id, config_paths,
        consent_to_claude_hosts, launch_invocation, new_session_id, progress_id, project_key,
        register_session_writer, remote_session_write_scope, session_environment, session_key,
        show_progress, spawn_terminal, take_session_slot,
    };

    /// Where the staged CLI and anything else this launcher leaves on the host
    /// lives (see `managed_claude_binary`), relative to the account's home.
    /// Protected for every session, so that one whose folder *is* the account's
    /// home cannot overwrite the very binary it runs: in WSL the helper adds
    /// this itself, but on an SSH host it is the client that has to ask.
    const HOST_CACHE_HOME_RELATIVE: &str = ".cache/zed-jp-enhance";

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
            workspace.show_error(
                format!("{LAUNCH_FAILED_PREFIX}プロジェクトは接続先のホストに繋がっていません。"),
                cx,
            );
            return;
        };
        show_progress(workspace, PREPARING_MESSAGE, cx);
        cx.spawn_in(window, async move |workspace, cx| {
            let outcome = start_session(
                workspace.clone(),
                project,
                remote_client,
                roots,
                key_roots,
                git_dirs,
                effort,
                cx,
            )
            .await;
            workspace
                .update(cx, |workspace, cx| {
                    workspace.dismiss_toast(&progress_id(), cx);
                    if let Err(error) = outcome {
                        workspace.show_error(format!("{LAUNCH_FAILED_PREFIX}{error:#}"), cx);
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
            bail!("Claude Code を実行するフォルダーがありません。");
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
        let remote_binary = remote_binary.context(
            "接続先のホストの Zed サーバーがまだ用意されていません。接続が終わるまで待ってから\
             もう一度お試しください。",
        )?;
        // Held until the session ends: a second one on the same configuration
        // would share its configuration directory and its login.
        let key = session_key(Some(&connection), &project_key);
        let Some(slot) = cx.update(|window, cx| take_session_slot(key, &workspace, window, cx))?
        else {
            return Ok(());
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
                log::warn!("[claude-sandbox] Claude Code を用意できませんでした: {error:#}");
                anyhow!("{error}")
            })?;
        let home = binary
            .home_on_host
            .context("接続先のホームディレクトリが分かりませんでした (内部エラー)。")?;
        let (config_root, config_dir) = config_paths(&home, &project_key)?;
        workspace.update(cx, |workspace, cx| {
            show_progress(workspace, PREPARING_SANDBOX_MESSAGE, cx)
        })?;
        // The helper skips a write grant that is not there yet (it logs and
        // carries on), so the CLI would start unable to write its own
        // configuration — and unable to keep a sign-in. In WSL the IDE bridge
        // creates these; on an SSH host there is no bridge, so it happens here.
        crate::managed_claude_binary::run_remote_host_script(
            remote_client.clone(),
            CREATE_CONFIG_SCRIPT,
            vec![config_root.clone(), config_dir.clone()],
            cx,
        )
        .await
        .context("接続先に Claude Code の設定フォルダーを作成できませんでした。")?;

        let session_id = new_session_id();
        let (program, args) = launch_invocation(&binary.path_on_host, effort.as_deref())?;
        let environment = session_environment(&config_dir, None);
        let first_root = roots
            .first()
            .context("Claude Code を実行するフォルダーがありません。")?
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
                            anyhow!("Claude Code の接続先が不正です (内部エラー): {error}")
                        })?,
                ),
            ),
            // The `.git` paths arrive spelled the way the host spells them
            // (`acp_thread::sandbox_git_dirs` uses the project's path style),
            // and the cache is built from the home the host reported.
            protected_paths: git_dirs
                .into_iter()
                .chain([PathBuf::from(format!("{home}/{HOST_CACHE_HOME_RELATIVE}"))])
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
            Some(&session_id),
            program,
            args,
        );

        // ADR-F50: the CLI writes its folders and its configuration from its
        // first moment, so it is their writer before it starts.
        let write_scope = remote_session_write_scope(&roots, &config_root);
        let (worktree_writer, worktree_writer_resolved) =
            cx.update(|_, cx| register_session_writer(&project, write_scope, cx))?;
        let resources = SessionResources {
            worktree_writer,
            slot,
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
        resources.slot.set_terminal(terminal.downgrade());
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

    /// What a running remote session holds on to. There is no bridge and no
    /// sandbox object on this machine: the helper on the host owns both its
    /// namespaces and its proxy, and the terminal's own process is the `ssh`
    /// that runs it.
    struct SessionResources {
        /// ADR-F50: the session's hold on the worktrees it can write.
        worktree_writer: SandboxedWriterGuard,
        /// Freed last, once nothing else of the session is left.
        slot: SessionSlot,
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
                worktree_writer,
                slot,
            } = resources;
            // The CLI has exited, so nothing of its writes is still to come.
            drop(worktree_writer);
            drop(slot);
            let exit_code = status
                .and_then(|status| status.code())
                .and_then(|code| u32::try_from(code).ok());
            let message = terminal
                .read_with(cx, |terminal, _| {
                    acp_thread::remote_sandbox_setup_failure_message(
                        exit_code,
                        &terminal.get_content(),
                    )
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
        HostsConsent, IdeLink, LAUNCH_FAILED_PREFIX, PREPARING_MESSAGE, PREPARING_SANDBOX_MESSAGE,
        PendingIdeSession, SessionLayout, SessionPlan, SessionSlot, already_running_id,
        consent_to_claude_hosts, helper_failure_message, new_session_id, plan_session, progress_id,
        project_key, register_session_writer, session_key, session_write_scope,
        show_already_running, show_progress, spawn_terminal, stale_entries, take_session_slot,
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
            bail!("Claude Code を実行するフォルダーがありません。");
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
                log::warn!("[claude-sandbox] Claude Code を用意できませんでした: {error:#}");
                anyhow!("{error}")
            })?;
        workspace.update(cx, |workspace, cx| {
            show_progress(workspace, PREPARING_SANDBOX_MESSAGE, cx)
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
            slot,
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
        resources.slot.set_terminal(terminal.downgrade());
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
                bridge_slot,
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
            // Before the slot: a bridge that ran past the timeout is still
            // there until this drop stops it, and handing its place on to the
            // next one first would let more `wsl.exe` run than the limit says.
            drop(bridge);
            drop(bridge_slot);
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
        PendingIdeSession, RunningSessions, SessionLayout, StaleEntries, config_paths,
        helper_failure_message, hosts_detail, launch_invocation, plan_session, project_key,
        register_session_writer, remote_session_write_scope, session_environment, session_key,
        session_write_scope, stale_entries,
    };
    use super::{
        CLAUDE_PROGRAM, COLLABORATION_REASON, HostLauncher, LaunchRoute, NO_FOLDER_REASON,
        OUTSIDE_SANDBOX_MESSAGE, OUTSIDE_SANDBOX_NOTICE, ProjectShape, REMOTE_CONNECTION_REASON,
        REMOTE_NOT_LINUX_REASON, RemoteShape, UNSUPPORTED_OS_REASON, outside_sandbox_detail,
        outside_sandbox_message, register_launch_actions, route_for,
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
            remote: None,
            has_folder: true,
            sandbox_platform: true,
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

    /// ADR-F53: this session is the official CLI signing itself in, which a
    /// Claude subscription may be used for; the ACP Claude agent drives the
    /// Agent SDK from this IDE's harness, which it may not. So the agent gets
    /// a folder of its own beside this session's, never the same one — this
    /// test fails the moment the two are one place again.
    #[test]
    fn the_acp_claude_agent_does_not_share_this_sessions_folder() {
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
        assert_ne!(agent_config_dir, PathBuf::from(&layout.config_dir));
    }

    #[test]
    fn the_outside_sandbox_confirmation_names_what_runs_where() {
        let local = outside_sandbox_detail(CLAUDE_PROGRAM, false, Some(NO_FOLDER_REASON));
        assert!(local.starts_with(OUTSIDE_SANDBOX_NOTICE), "{local}");
        assert!(local.contains("この PC の `claude`"), "{local}");
        assert!(local.ends_with(NO_FOLDER_REASON), "{local}");

        let remote = outside_sandbox_detail(CLAUDE_PROGRAM, true, Some(REMOTE_CONNECTION_REASON));
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

    /// 第 3 段: the session on an SSH host is placed by the same rule as the
    /// WSL one — the account's home decides, and the project's key names the
    /// folder — so the two never see each other's configuration, and a session
    /// gets no IDE channel it cannot serve.
    #[test]
    fn a_remote_session_is_placed_like_the_wsl_one_and_has_no_ide_channel() {
        let (config_root, config_dir) =
            config_paths("/home/ubuntu", PROJECT_KEY).expect("a valid placement");
        assert_eq!(
            config_root,
            "/home/ubuntu/.zed-jp-enhance-claude/projects/0123456789abcdef"
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

        let with_ide = session_environment(&config_dir, Some(45123));
        let without_ide = session_environment(&config_dir, None);
        let names = |environment: &[(String, String)]| {
            let mut names = environment
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        // Nothing points the CLI at an IDE when there is none to point it at:
        // with the port set it would wait for a lock file that never arrives.
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

    /// The launch itself is shared: the shell that sets what only exists inside
    /// the sandbox and then becomes the CLI by absolute path, never by `PATH`.
    #[test]
    fn the_launch_never_looks_the_cli_up_on_path() {
        let (program, args) =
            launch_invocation(CLAUDE_PATH, Some("high")).expect("an absolute path");
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
            launch_invocation(CLAUDE_PATH, None)
                .expect("an absolute path")
                .1
                .get(2..),
            Some(&[CLAUDE_PATH.to_string()][..])
        );
        for bad in ["claude", "~/claude", "/tmp/cl\naude"] {
            assert!(launch_invocation(bad, None).is_err(), "{bad:?}");
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
            super::launch_on_host_in_tab(workspace, effort, fallback.as_ref(), window, cx);
        });
        register_and_focus(&workspace, launcher, &mut cx);

        cx.dispatch_action(LaunchClaudeCodeOutsideSandbox {
            effort: Some("high".to_string()),
        });
        cx.run_until_parked();
        assert!(cx.has_pending_prompt(), "起動のたびに確認する");
        cx.simulate_prompt_answer("起動 (今回のみ)");
        cx.run_until_parked();

        assert_eq!(
            *fallbacks.borrow(),
            [Some("high".to_string())],
            "パネルが無ければターミナルパネル経路へ、引数もそのまま"
        );
    }

    #[gpui::test]
    async fn saying_that_the_session_already_runs_goes_away_by_itself(cx: &mut TestAppContext) {
        use super::session::{already_running_id, take_session_slot};

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
