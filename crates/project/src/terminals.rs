use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};

use futures::{FutureExt, future::Shared};
use itertools::Itertools as _;
use language::LanguageName;
use remote::{Interactive, RemoteClient, RemoteConnectionOptions};
use settings::{Settings, SettingsLocation};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use task::{Shell, ShellBuilder, ShellKind, SpawnInTerminal};
use terminal::{
    Terminal, TerminalBuilder, TerminalMode, insert_zed_terminal_env,
    remote_cwd::{osc7_prompt_command_env, shell_command_with_cwd_fallback},
    remote_session::{RemoteTerminalSession, persistent_session_command},
    terminal_settings::{RemotePersistenceSettings, TerminalSettings},
};
use util::{
    command::new_std_command, get_default_system_shell, get_system_shell, maybe, paths::PathStyle,
    rel_path::RelPath,
};

use crate::{Project, ProjectPath};

#[derive(Default)]
pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
    /// The remote server binary that turned out not to know
    /// `terminal-session`. Shells stop asking it for persistent sessions, until
    /// a different binary is installed on a later connection.
    persistence_unsupported_binary: Option<Arc<RelPath>>,
}

/// Whether an interactive shell asks the remote server for a persistent
/// session (see [`terminal::remote_session`]).
#[derive(Clone, Debug, PartialEq, Eq)]
enum RemoteSessionRequest {
    Off,
    /// A new session, or — when the id names one — that one again. The id
    /// comes from a terminal that was attached to it before a disconnect or a
    /// restart of Zed.
    Attach {
        resume_session_id: Option<String>,
    },
}

impl RemoteSessionRequest {
    /// The session the shell attaches to on the connection as `host` describes
    /// it. Without a known server binary (nothing installed yet, or no
    /// connection) the shell starts without one, and so does every restart of
    /// its tab; nothing is recorded against the binary installed later.
    fn launch(
        self,
        host: RemoteSessionHost,
        settings: &RemotePersistenceSettings,
        unsupported_binary: Option<&RelPath>,
    ) -> Option<PersistentSessionLaunch> {
        let Self::Attach { resume_session_id } = self else {
            return None;
        };
        let RemoteSessionHost {
            is_ssh,
            path_style,
            binary,
        } = host;
        if binary.is_none() && settings.enabled && is_ssh && !path_style.is_windows() {
            log::info!(
                "starting a remote shell without a persistent session: \
                 the remote server binary is not known on this connection"
            );
        }
        let binary =
            persistent_session_binary(is_ssh, path_style, binary, settings, unsupported_binary)?;
        Some(PersistentSessionLaunch::new(
            binary,
            resume_session_id,
            settings,
        ))
    }
}

/// What a persistent session depends on in the connection a shell starts on.
#[derive(Clone, Debug)]
struct RemoteSessionHost {
    is_ssh: bool,
    path_style: PathStyle,
    binary: Option<Arc<RelPath>>,
}

impl RemoteSessionHost {
    fn of(remote_client: &RemoteClient) -> Self {
        Self {
            is_ssh: matches!(
                remote_client.connection_options(),
                RemoteConnectionOptions::Ssh(_)
            ),
            path_style: remote_client.path_style(),
            binary: remote_client.remote_server_binary_path(),
        }
    }
}

/// A persistent session a new remote shell attaches to, as decided before
/// the `ssh` command is built.
#[derive(Clone, Debug)]
struct PersistentSessionLaunch {
    binary: Arc<RelPath>,
    session: RemoteTerminalSession,
    detached_timeout_secs: u64,
}

impl PersistentSessionLaunch {
    fn new(
        binary: Arc<RelPath>,
        resume_session_id: Option<String>,
        settings: &RemotePersistenceSettings,
    ) -> Self {
        // An id the server would refuse (a damaged database row) must not
        // make the attach fail, which would read as a server without
        // sessions; the shell just starts over in a new one.
        let resume_session_id = resume_session_id.filter(|id| is_valid_session_id(id));
        let resume = resume_session_id.is_some();
        let id = resume_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Self {
            binary,
            session: RemoteTerminalSession {
                id,
                // Fresh for every attach, so that output of an earlier attach
                // replayed into this one cannot end this one's replay early.
                nonce: uuid::Uuid::new_v4().simple().to_string(),
                resume,
            },
            detached_timeout_secs: settings.detached_timeout_secs(),
        }
    }
}

/// Whether the shell is one left running in a session from before, which a
/// toolchain activation typed into it could interrupt.
fn resumes_session(persistent_session: Option<&PersistentSessionLaunch>) -> bool {
    persistent_session.is_some_and(|launch| launch.session.resume)
}

/// What `terminal-session` accepts as an id: `[A-Za-z0-9_-]{1,64}`.
fn is_valid_session_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

/// The remote server binary to ask for a persistent session, when a shell
/// on this connection can have one: only over SSH (WSL and containers do not
/// drop), only on a POSIX host (the holder is POSIX-only), and not from a
/// binary that already showed it does not know the subcommand.
fn persistent_session_binary(
    is_ssh: bool,
    path_style: PathStyle,
    binary: Option<Arc<RelPath>>,
    settings: &RemotePersistenceSettings,
    unsupported_binary: Option<&RelPath>,
) -> Option<Arc<RelPath>> {
    if !settings.enabled || !is_ssh || path_style.is_windows() {
        return None;
    }
    let binary = binary?;
    if unsupported_binary == Some(binary.as_ref()) {
        return None;
    }
    Some(binary)
}

/// Zed が起こすターミナルに環境変数を足すためのフック。
///
/// `project` は上位のクレート (内蔵 IDE サーバー等) に依存できないので、変数を足したい
/// 側が起動時に自分を登録する。
pub type TerminalEnvironmentProvider =
    Arc<dyn Fn(&Entity<Project>, &App) -> Vec<(String, String)> + Send + Sync>;

#[derive(Default)]
struct TerminalEnvironmentProviders(Vec<TerminalEnvironmentProvider>);

impl Global for TerminalEnvironmentProviders {}

pub fn register_terminal_environment_provider(
    provider: impl Fn(&Entity<Project>, &App) -> Vec<(String, String)> + Send + Sync + 'static,
    cx: &mut App,
) {
    cx.default_global::<TerminalEnvironmentProviders>()
        .0
        .push(Arc::new(provider));
}

/// 登録済みのプロバイダーがこのターミナルに足す環境変数。
///
/// リモート越しのターミナルには何も足さない: 値が指しているのは Zed が動いている
/// この PC の loopback であって、リモートホストには存在しないため。
pub fn provided_terminal_environment(
    project: &Entity<Project>,
    is_via_remote: bool,
    cx: &App,
) -> Vec<(String, String)> {
    if is_via_remote {
        return Vec::new();
    }
    let Some(providers) = cx.try_global::<TerminalEnvironmentProviders>() else {
        return Vec::new();
    };
    providers
        .0
        .iter()
        .flat_map(|provider| provider(project, cx))
        .collect()
}

/// The variables every Zed terminal gets (`TERM`, `ZED_TERM`, ...), for a
/// sandboxed remote agent command to receive inside its sandbox (ADR-F46).
pub fn zed_terminal_environment(cx: &App) -> HashMap<String, String> {
    let mut environment = HashMap::default();
    insert_zed_terminal_env(&mut environment, &release_channel::AppVersion::global(cx));
    environment
}

/// What a task terminal adds of its own around the task it spawns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskSpawnEnvironment {
    /// Everything, as upstream: the directory environment, `terminal.env`,
    /// registered providers and toolchain activation.
    Full,
    /// Nothing: an agent command already wrapped in a sandbox (ADR-F46), whose
    /// wrapper process all of that would reach before the sandbox exists.
    SandboxWrapped,
}

impl TaskSpawnEnvironment {
    fn activates_toolchain(self, detect_venv: bool) -> bool {
        self == Self::Full && detect_venv
    }
}

/// The environment a task terminal's process starts with, before the task's
/// own `env` is laid over it.
fn terminal_process_environment(
    spawn_environment: TaskSpawnEnvironment,
    directory_environment: HashMap<String, String>,
    provided_environment: Vec<(String, String)>,
    settings_environment: impl IntoIterator<Item = (String, String)>,
) -> HashMap<String, String> {
    match spawn_environment {
        TaskSpawnEnvironment::Full => {
            let mut environment = directory_environment;
            environment.extend(provided_environment);
            // 設定の `terminal.env` が明示した値のほうが強い。
            environment.extend(settings_environment);
            environment
        }
        TaskSpawnEnvironment::SandboxWrapped => HashMap::default(),
    }
}

impl Project {
    /// ADR-F46: the `terminal.env` from settings that applies in `directory`,
    /// for a sandboxed agent command to set inside its sandbox, since
    /// [`Self::create_sandboxed_terminal_task`] puts nothing on the process it
    /// starts.
    pub fn terminal_settings_environment(
        &self,
        directory: Option<&Path>,
        cx: &App,
    ) -> HashMap<String, String> {
        let settings_location = directory
            .and_then(|directory| self.find_worktree(directory, cx))
            .map(|(worktree, _)| SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        TerminalSettings::get(settings_location, cx)
            .env
            .clone()
            .into_iter()
            .collect()
    }

    pub fn active_entry_directory(&self, cx: &App) -> Option<PathBuf> {
        let entry_id = self.active_entry()?;
        let worktree = self.worktree_for_entry(entry_id, cx)?;
        let worktree = worktree.read(cx);
        let entry = worktree.entry_for_id(entry_id)?;

        let absolute_path = worktree.absolutize(entry.path.as_ref());
        if entry.is_dir() {
            Some(absolute_path)
        } else {
            absolute_path.parent().map(|p| p.to_path_buf())
        }
    }

    pub fn active_project_directory(&self, cx: &App) -> Option<Arc<Path>> {
        self.active_entry()
            .and_then(|entry_id| self.worktree_for_entry(entry_id, cx))
            .into_iter()
            .chain(self.worktrees(cx))
            .find_map(|tree| tree.read(cx).root_dir())
    }

    pub fn first_project_directory(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.worktrees(cx).next()?;
        let worktree = worktree.read(cx);
        if worktree.root_entry()?.is_dir() {
            Some(worktree.abs_path().to_path_buf())
        } else {
            None
        }
    }

    pub fn create_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.spawn_terminal_task(spawn_task, TaskSpawnEnvironment::Full, cx)
    }

    /// ADR-F46: spawns an agent command that is already wrapped in a sandbox.
    ///
    /// Nothing of the terminal's own goes onto the process it starts — no
    /// directory environment, no `terminal.env`, no registered providers, no
    /// toolchain activation — because all of that runs before the wrapper,
    /// outside the sandbox: an activation script the sandboxed command can
    /// rewrite, or an `LD_PRELOAD` from the project's `.zed/settings.json`,
    /// would execute with the editor's own privileges. The caller applies what
    /// the command should see inside the sandbox instead (see
    /// [`Self::terminal_settings_environment`]). On a remote, a task without a
    /// `cwd` starts in the remote home directory, which the sandbox helper's
    /// path is relative to.
    pub fn create_sandboxed_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.spawn_terminal_task(spawn_task, TaskSpawnEnvironment::SandboxWrapped, cx)
    }

    /// The environment a task terminal starts from before its task's own `env`:
    /// the directory environment, which runs the user's login shell to capture,
    /// and what registered providers add. A sandbox-wrapped spawn asks for
    /// neither.
    fn terminal_environment_sources(
        &mut self,
        spawn_environment: TaskSpawnEnvironment,
        shell: &str,
        path: Option<Arc<Path>>,
        remote_client: Option<Entity<RemoteClient>>,
        cx: &mut Context<Self>,
    ) -> (
        Shared<Task<Option<HashMap<String, String>>>>,
        Vec<(String, String)>,
    ) {
        match spawn_environment {
            TaskSpawnEnvironment::Full => {
                let is_via_remote = remote_client.is_some();
                (
                    self.resolve_directory_environment(shell, path, remote_client, cx),
                    provided_terminal_environment(&cx.entity(), is_via_remote, cx),
                )
            }
            TaskSpawnEnvironment::SandboxWrapped => (Task::ready(None).shared(), Vec::new()),
        }
    }

    fn spawn_terminal_task(
        &mut self,
        spawn_task: SpawnInTerminal,
        spawn_environment: TaskSpawnEnvironment,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let is_via_remote = self.remote_client.is_some();

        let path: Option<Arc<Path>> = if let Some(cwd) = &spawn_task.cwd {
            if is_via_remote {
                Some(Arc::from(cwd.as_ref()))
            } else {
                let cwd = cwd.to_string_lossy();
                let tilde_substituted = shellexpand::tilde(&cwd);
                Some(Arc::from(Path::new(tilde_substituted.as_ref())))
            }
        } else if is_via_remote && spawn_environment == TaskSpawnEnvironment::SandboxWrapped {
            None
        } else {
            self.active_project_directory(cx)
        };

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv =
            spawn_environment.activates_toolchain(settings.detect_venv.as_option().is_some());

        let terminal_mode = TerminalMode::task(spawn_task.clone());

        let local_path = if is_via_remote { None } else { path.clone() };
        let remote_client = self.remote_client.clone();
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => get_system_shell(),
        };
        let path_style = self.path_style(cx);
        let shell_kind = ShellKind::new(&shell, path_style.is_windows());

        // Prepare a task for resolving the environment
        let (env_task, provided_environment) = self.terminal_environment_sources(
            spawn_environment,
            &shell,
            path.clone(),
            remote_client.clone(),
            cx,
        );

        // Scope the toolchain lookup to the worktree the terminal is being
        // spawned in. Previously this iterated the active editor's worktree
        // and then every visible worktree, so a Python toolchain persisted
        // for worktree A would leak into a terminal opened in worktree B and
        // inject (e.g.) `conda activate base` into a shell that has no
        // business with conda.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let lang_registry = self.languages.clone();
        cx.spawn(async move |project, cx| {
            let mut env = terminal_process_environment(
                spawn_environment,
                env_task.await.unwrap_or_default(),
                provided_environment,
                settings.env,
            );

            let activation_script = maybe!(async {
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            let builder = project
                .update(cx, move |_, cx| {
                    let format_to_run = |spawn_task: &SpawnInTerminal| {
                        format_task_for_activation(
                            spawn_task,
                            shell_kind,
                            &shell,
                            path_style.is_windows(),
                        )
                    };

                    let (shell, env) = {
                        let to_run =
                            (!activation_script.is_empty()).then(|| format_to_run(&spawn_task));
                        env.extend(spawn_task.env);
                        match remote_client {
                            Some(remote_client) => match activation_script.clone() {
                                activation_script if !activation_script.is_empty() => {
                                    let separator = shell_kind.sequential_commands_separator();
                                    let activation_script =
                                        activation_script.join(&format!("{separator} "));
                                    let to_run = to_run.expect("activation command was formatted");

                                    let arg = format!("{activation_script}{separator} {to_run}");
                                    let args = shell_kind.args_for_shell(true, arg);
                                    let shell = remote_client
                                        .read(cx)
                                        .shell()
                                        .unwrap_or_else(get_default_system_shell);

                                    create_remote_shell(
                                        Some((&shell, &args)),
                                        env,
                                        path,
                                        remote_client,
                                        cx,
                                    )?
                                }
                                _ => {
                                    let (shell, env, _) = spawn_remote_command(
                                        spawn_task
                                            .command
                                            .as_ref()
                                            .map(|command| (command, &spawn_task.args)),
                                        env,
                                        path,
                                        remote_client,
                                        spawn_environment,
                                        None,
                                        cx,
                                    )?;
                                    (shell, env)
                                }
                            },
                            None => match activation_script.clone() {
                                activation_script if !activation_script.is_empty() => {
                                    let separator = shell_kind.sequential_commands_separator();
                                    let activation_script =
                                        activation_script.join(&format!("{separator} "));
                                    let to_run = to_run.expect("activation command was formatted");

                                    let arg = format!("{activation_script}{separator} {to_run}");
                                    let args = shell_kind.args_for_shell(true, arg);

                                    (
                                        Shell::WithArguments {
                                            program: shell,
                                            args,
                                            title_override: None,
                                        },
                                        env,
                                    )
                                }
                                _ => (
                                    if let Some(program) = spawn_task.command {
                                        Shell::WithArguments {
                                            program,
                                            args: spawn_task.args,
                                            title_override: None,
                                        }
                                    } else {
                                        Shell::System
                                    },
                                    env,
                                ),
                            },
                        }
                    };
                    anyhow::Ok(TerminalBuilder::new(
                        local_path.map(|path| path.to_path_buf()),
                        terminal_mode,
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        cx,
                        activation_script,
                        path_style,
                    ))
                })??
                .await?;
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn create_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.create_terminal_shell_internal(cwd, false, RemoteSessionRequest::Off, cx)
    }

    /// [`Self::create_terminal_shell`] for a terminal tab, whose remote shell
    /// survives a dropped SSH connection when `terminal.remote_persistence`
    /// allows it: it runs in a session the remote server holds, which
    /// `resume_session_id` names when the tab was attached to one before.
    /// Anywhere else this is just [`Self::create_terminal_shell`].
    ///
    /// Only for shells in tabs, because closing the tab is what ends the
    /// session ([`Self::kill_remote_terminal_session`]); anything else would
    /// leave it running on the host until it times out.
    pub fn create_persistent_terminal_shell(
        &mut self,
        cwd: Option<PathBuf>,
        resume_session_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        self.create_terminal_shell_internal(
            cwd,
            false,
            RemoteSessionRequest::Attach { resume_session_id },
            cx,
        )
    }

    /// Ends a persistent session on the remote host because its tab was
    /// closed. Failure only costs the host a shell until the session's
    /// detached timeout, so it is logged rather than reported.
    pub fn kill_remote_terminal_session(&self, session_id: String, cx: &App) {
        let Some(remote_client) = &self.remote_client else {
            return;
        };
        let request =
            remote_client
                .read(cx)
                .proto_client()
                .request(rpc::proto::KillTerminalSession {
                    project_id: rpc::proto::REMOTE_SERVER_PROJECT_ID,
                    session_id: session_id.clone(),
                });
        cx.background_spawn(async move {
            if let Err(error) = request.await {
                log::warn!("could not end remote terminal session {session_id}: {error:#}");
            }
        })
        .detach();
    }

    /// Stops asking the connected remote server for persistent sessions: a
    /// shell's attach exited without reaching a holder, which is what a server
    /// that predates them does.
    pub fn disable_remote_terminal_persistence(&mut self, cx: &App) {
        let binary = self
            .remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).remote_server_binary_path());
        log::warn!(
            "the remote server {binary:?} does not support persistent terminal sessions; \
             starting remote shells without them"
        );
        self.terminals.persistence_unsupported_binary = binary;
    }

    /// Creates a local terminal even if the project is remote.
    /// In remote projects: opens in Zed's launch directory (bypasses SSH).
    /// In local projects: opens in the project directory (same as regular terminals).
    pub fn create_local_terminal(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let working_directory = if self.remote_client.is_some() {
            // Remote project: don't use remote paths, let shell use Zed's cwd
            None
        } else {
            // Local project: use project directory like normal terminals
            self.active_project_directory(cx).map(|p| p.to_path_buf())
        };
        self.create_terminal_shell_internal(working_directory, true, RemoteSessionRequest::Off, cx)
    }

    /// Internal method for creating terminal shells.
    /// If force_local is true, creates a local terminal even if the project has a remote client.
    /// This allows "breaking out" to a local shell in remote projects.
    fn create_terminal_shell_internal(
        &mut self,
        cwd: Option<PathBuf>,
        force_local: bool,
        session_request: RemoteSessionRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Terminal>>> {
        let path = cwd.map(|p| Arc::from(&*p));
        let is_via_remote = !force_local && self.remote_client.is_some();

        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        let settings = TerminalSettings::get(settings_location, cx).clone();
        let detect_venv = settings.detect_venv.as_option().is_some();
        let local_path = if is_via_remote { None } else { path.clone() };
        // The counterpart of `local_path`: a remote shell's directory is a path
        // on the host, which only the terminal's OSC 7 tracker may hold.
        let remote_path = if is_via_remote {
            path.as_deref().map(Path::to_path_buf)
        } else {
            None
        };

        // See create_terminal_task: scope the toolchain lookup to the
        // worktree the terminal is opened in, not the active editor's
        // worktree or other visible worktrees.
        let project_path_contexts: Vec<ProjectPath> = path
            .as_ref()
            .and_then(|p| self.find_worktree(p, cx))
            .map(|(worktree, relative_path)| ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path: relative_path,
            })
            .into_iter()
            .collect();
        let toolchains = project_path_contexts
            .into_iter()
            .filter(|_| detect_venv)
            .map(|p| self.active_toolchain(p, LanguageName::new_static("Python"), cx))
            .collect::<Vec<_>>();
        let remote_client = if force_local {
            None
        } else {
            self.remote_client.clone()
        };
        let shell = match &remote_client {
            Some(remote_client) => remote_client
                .read(cx)
                .shell()
                .unwrap_or_else(get_default_system_shell),
            None => settings.shell.program(),
        };
        let env_shell = match &remote_client {
            Some(_) => shell.clone(),
            None => get_system_shell(),
        };

        let path_style = self.path_style(cx);

        // Prepare a task for resolving the environment
        let env_task =
            self.resolve_directory_environment(&env_shell, path.clone(), remote_client.clone(), cx);

        let lang_registry = self.languages.clone();
        let provided_environment = provided_terminal_environment(&cx.entity(), is_via_remote, cx);
        cx.spawn(async move |project, cx| {
            let shell_kind = ShellKind::new(&shell, path_style.is_windows());
            let mut env = env_task.await.unwrap_or_default();
            env.extend(provided_environment);
            env.extend(settings.env);

            // Not decided when the tab is opened: while the connection is
            // being re-established there is no server binary to ask, and the
            // shell itself only starts once it is back (the environment of a
            // remote directory comes from the server).
            let persistent_session = match &remote_client {
                Some(remote_client) => project.update(cx, |project, cx| {
                    session_request.launch(
                        RemoteSessionHost::of(remote_client.read(cx)),
                        &settings.remote_persistence,
                        project.terminals.persistence_unsupported_binary.as_deref(),
                    )
                })?,
                None => None,
            };
            let resumes_session = resumes_session(persistent_session.as_ref());
            let activation_script = maybe!(async {
                // Typed into the shell once it starts, which for a resumed
                // session may already be running something else.
                if resumes_session {
                    return None;
                }
                for toolchain in toolchains {
                    let Some(toolchain) = toolchain.await else {
                        continue;
                    };
                    let language = lang_registry
                        .language_for_name(&toolchain.language_name.0)
                        .await
                        .ok();
                    let lister = language?.toolchain_lister()?;
                    let future =
                        cx.update(|cx| lister.activation_script(&toolchain, shell_kind, cx));
                    return Some(future.await);
                }
                None
            })
            .await
            .unwrap_or_default();

            let builder = project
                .update(cx, move |_, cx| {
                    let (shell, env, remote_session) = {
                        match remote_client {
                            Some(remote_client) => spawn_remote_command(
                                None,
                                env,
                                path,
                                remote_client,
                                TaskSpawnEnvironment::Full,
                                persistent_session.as_ref(),
                                cx,
                            )?,
                            None => (settings.shell, env, None),
                        }
                    };
                    anyhow::Ok(TerminalBuilder::new_with_remote_session(
                        local_path.map(|path| path.to_path_buf()),
                        TerminalMode::interactive(),
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        Duration::from_millis(settings.path_hyperlink_timeout_ms),
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        cx,
                        activation_script,
                        path_style,
                        remote_session,
                    ))
                })??
                .await?
                .with_remote_working_directory(remote_path);
            project.update(cx, move |this, cx| {
                let terminal_handle = cx.new(|cx| builder.subscribe(cx));

                this.terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn clone_terminal(
        &mut self,
        terminal: &Entity<Terminal>,
        cx: &mut Context<'_, Project>,
        cwd: Option<PathBuf>,
    ) -> Task<Result<Entity<Terminal>>> {
        // We cannot clone the task's terminal, as it will effectively re-spawn the task, which might not be desirable.
        // For now, create a new shell instead.
        if terminal.read(cx).task().is_some() {
            return self.create_terminal_shell(cwd, cx);
        }
        // A remote terminal's cwd is baked into its ssh command line, so the
        // builder clone below cannot move it; when the shell has reported where
        // it went (OSC 7), build a fresh command around that directory instead.
        if self.is_via_remote_server() {
            let terminal = terminal.read(cx);
            // The command line of a persistent session's terminal names that
            // session, and running it again would take the shell away from
            // the terminal being cloned.
            if terminal.remote_session_id().is_some() {
                let remote_cwd = terminal.remote_working_directory();
                return self.create_persistent_terminal_shell(remote_cwd, None, cx);
            }
            if let Some(remote_cwd) = terminal.remote_working_directory() {
                return self.create_terminal_shell(Some(remote_cwd), cx);
            }
        }
        let local_path = if self.is_via_remote_server() {
            None
        } else {
            cwd
        };

        let builder = terminal.read(cx).clone_builder(cx, local_path);
        cx.spawn(async |project, cx| {
            let terminal = builder.await?;
            project.update(cx, |project, cx| {
                let terminal_handle = cx.new(|cx| terminal.subscribe(cx));

                project
                    .terminals
                    .local_handles
                    .push(terminal_handle.downgrade());

                let id = terminal_handle.entity_id();
                cx.observe_release(&terminal_handle, move |project, _terminal, cx| {
                    let handles = &mut project.terminals.local_handles;

                    if let Some(index) = handles
                        .iter()
                        .position(|terminal| terminal.entity_id() == id)
                    {
                        handles.remove(index);
                        cx.notify();
                    }
                })
                .detach();

                terminal_handle
            })
        })
    }

    pub fn terminal_settings<'a>(
        &'a self,
        path: &'a Option<PathBuf>,
        cx: &'a App,
    ) -> &'a TerminalSettings {
        let mut settings_location = None;
        if let Some(path) = path.as_ref()
            && let Some((worktree, _)) = self.find_worktree(path, cx)
        {
            settings_location = Some(SettingsLocation {
                worktree_id: worktree.read(cx).id(),
                path: RelPath::empty(),
            });
        }
        TerminalSettings::get(settings_location, cx)
    }

    pub fn exec_in_shell(
        &self,
        command: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<smol::process::Command>> {
        let path = self.first_project_directory(cx);
        let remote_client = self.remote_client.clone();
        let settings = self.terminal_settings(&path, cx).clone();
        let shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell())
            .map(Shell::Program)
            .unwrap_or(Shell::System);
        let is_windows = self.path_style(cx).is_windows();
        let builder = ShellBuilder::new(&shell, is_windows).non_interactive();
        let (command, args) = builder.build(Some(command), &Vec::new());

        let env_task = self.resolve_directory_environment(
            &shell.program(),
            path.as_ref().map(|p| Arc::from(&**p)),
            remote_client.clone(),
            cx,
        );

        cx.spawn(async move |project, cx| {
            let mut env = env_task.await.unwrap_or_default();
            env.extend(settings.env);

            project.update(cx, move |_, cx| {
                match remote_client {
                    Some(remote_client) => {
                        let command_template = remote_client.read(cx).build_command(
                            Some(command),
                            &args,
                            &env,
                            None,
                            None,
                            Interactive::Yes,
                        )?;
                        let mut command = new_std_command(command_template.program);
                        command.args(command_template.args);
                        command.envs(command_template.env);
                        Ok(command)
                    }
                    None => {
                        let mut command = new_std_command(command);
                        command.args(args);
                        command.envs(env);
                        if let Some(path) = path {
                            command.current_dir(path);
                        }
                        Ok(command)
                    }
                }
                .map(|mut process| {
                    util::set_pre_exec_to_start_new_session(&mut process);
                    smol::process::Command::from(process)
                })
            })?
        })
    }

    pub fn local_terminal_handles(&self) -> &Vec<WeakEntity<terminal::Terminal>> {
        &self.terminals.local_handles
    }

    /// Lists a terminal built without a PTY as one of this project's terminals.
    #[cfg(any(test, feature = "test-support"))]
    pub fn add_terminal_handle_for_test(&mut self, terminal: &Entity<Terminal>) {
        self.terminals.local_handles.push(terminal.downgrade());
    }

    fn resolve_directory_environment(
        &self,
        shell: &str,
        path: Option<Arc<Path>>,
        remote_client: Option<Entity<RemoteClient>>,
        cx: &mut App,
    ) -> Shared<Task<Option<HashMap<String, String>>>> {
        if let Some(path) = &path {
            let shell = Shell::Program(shell.to_string());
            self.environment
                .update(cx, |project_env, cx| match &remote_client {
                    Some(remote_client) => project_env.remote_directory_environment(
                        &shell,
                        path.clone(),
                        remote_client.clone(),
                        cx,
                    ),
                    None => project_env.local_directory_environment(&shell, path.clone(), cx),
                })
        } else {
            Task::ready(None).shared()
        }
    }
}

fn create_remote_shell(
    spawn_command: Option<(&String, &Vec<String>)>,
    env: HashMap<String, String>,
    working_directory: Option<Arc<Path>>,
    remote_client: Entity<RemoteClient>,
    cx: &mut App,
) -> Result<(Shell, HashMap<String, String>)> {
    let (shell, env, _) = spawn_remote_command(
        spawn_command,
        env,
        working_directory,
        remote_client,
        TaskSpawnEnvironment::Full,
        None,
        cx,
    )?;
    Ok((shell, env))
}

/// Builds the `ssh` command for a remote process. For an interactive shell
/// (`spawn_command: None`) with a `persistent_session`, the shell is started
/// through the session, which is then returned for the terminal to attach to.
fn spawn_remote_command(
    spawn_command: Option<(&String, &Vec<String>)>,
    mut env: HashMap<String, String>,
    mut working_directory: Option<Arc<Path>>,
    remote_client: Entity<RemoteClient>,
    spawn_environment: TaskSpawnEnvironment,
    persistent_session: Option<&PersistentSessionLaunch>,
    cx: &mut App,
) -> Result<(
    Shell,
    HashMap<String, String>,
    Option<RemoteTerminalSession>,
)> {
    if spawn_environment == TaskSpawnEnvironment::Full {
        insert_zed_terminal_env(&mut env, &release_channel::AppVersion::global(cx));
    }

    let mut remote_session = None;
    let (program, args) = match spawn_command {
        Some((program, args)) => (Some(program.clone()), args.clone()),
        None => {
            // Only an interactive shell has a prompt to hang the report off, and
            // only an interactive shell is ever restarted after a dropped
            // connection.
            let remote_shell = remote_client.read(cx).shell();
            if let Some(remote_shell) = &remote_shell
                && let Some((name, value)) = osc7_prompt_command_env(remote_shell)
            {
                env.entry(name).or_insert(value);
            }
            // A shell restarted in a directory tracked earlier may find it
            // gone, and entering it must not take the shell down with it. The
            // wrapper does the `cd` itself, so the command line `ssh` builds
            // must not do it a second time.
            let cwd_fallback = remote_shell
                .clone()
                .filter(|_| !remote_client.read(cx).path_style().is_windows())
                .zip(working_directory.as_deref())
                .and_then(|(remote_shell, directory)| {
                    shell_command_with_cwd_fallback(directory, &remote_shell)
                });
            let (program, args) = match cwd_fallback {
                Some((program, args)) => {
                    working_directory = None;
                    (Some(program), args)
                }
                None => (None, Vec::new()),
            };
            match persistent_session_shell_command(
                persistent_session,
                remote_shell,
                working_directory.as_deref(),
                program,
                args,
            ) {
                PersistentShellCommand::Attached {
                    program,
                    args,
                    session,
                } => {
                    remote_session = Some(session);
                    // For the notices the attach and its holder write into
                    // the terminal; the holder keeps it from its shell.
                    env.insert(
                        remote::DISPLAY_LANGUAGE_ENV_VAR.to_string(),
                        i18n::current_locale().to_string(),
                    );
                    (Some(program), args)
                }
                PersistentShellCommand::Plain { program, args } => (program, args),
            }
        }
    };

    let command = remote_client.read(cx).build_command(
        program,
        args.as_slice(),
        &env,
        working_directory.map(|path| path.display().to_string()),
        None,
        Interactive::Yes,
    )?;

    log::debug!("Connecting to a remote server: {:?}", command.program);
    let host = remote_client.read(cx).connection_options().display_name();

    Ok((
        Shell::WithArguments {
            program: command.program,
            args: command.args,
            title_override: Some(format!("{} — Terminal", host)),
        },
        command.env,
        remote_session,
    ))
}

enum PersistentShellCommand {
    Attached {
        program: String,
        args: Vec<String>,
        session: RemoteTerminalSession,
    },
    Plain {
        program: Option<String>,
        args: Vec<String>,
    },
}

/// Wraps an interactive shell's command (`None` = the login shell `ssh`
/// starts by default) in the attach to `persistent_session`, when there is
/// one and the command can be wrapped.
///
/// The server binary's path is relative to the home directory, which is
/// where the command line starts only when it is given no directory of its
/// own. A directory is always taken over by the cwd fallback wrapper, whose
/// `cd` then runs inside the session; a directory left over here means that
/// wrapper could not be built, and the shell starts without a session.
fn persistent_session_shell_command(
    persistent_session: Option<&PersistentSessionLaunch>,
    remote_shell: Option<String>,
    working_directory: Option<&Path>,
    program: Option<String>,
    args: Vec<String>,
) -> PersistentShellCommand {
    let Some(launch) = persistent_session.filter(|_| working_directory.is_none()) else {
        return PersistentShellCommand::Plain { program, args };
    };
    let (program, args) = match (program, remote_shell) {
        (Some(program), _) => (program, args),
        (None, Some(remote_shell)) => (remote_shell, vec!["-l".to_string()]),
        (None, None) => {
            return PersistentShellCommand::Plain {
                program: None,
                args,
            };
        }
    };
    let (program, args) = persistent_session_command(
        &launch.binary.display(PathStyle::Unix),
        &launch.session,
        launch.detached_timeout_secs,
        program,
        args,
    );
    PersistentShellCommand::Attached {
        program,
        args,
        session: launch.session.clone(),
    }
}

fn format_task_for_activation(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    shell: &str,
    is_windows: bool,
) -> String {
    if let Some(command) = &spawn_task.command {
        let command = shell_kind.prepend_command_prefix(command);
        let command = shell_kind.try_quote_prefix_aware(&command);
        let args = spawn_task
            .args
            .iter()
            .enumerate()
            .filter_map(|(index, arg)| {
                quote_prepared_task_arg_for_activation(
                    spawn_task, shell_kind, arg, index, is_windows,
                )
            });

        command.into_iter().chain(args).join(" ")
    } else {
        // todo: this breaks for remotes to windows
        format!("exec {shell} -l")
    }
}

fn quote_prepared_task_arg_for_activation<'a>(
    spawn_task: &SpawnInTerminal,
    shell_kind: ShellKind,
    arg: &'a str,
    index: usize,
    is_windows: bool,
) -> Option<Cow<'a, str>> {
    if spawn_task.shell.shell_kind(is_windows) == ShellKind::Cmd
        && index >= 2
        && spawn_task
            .args
            .get(index - 2)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/S"))
        && spawn_task
            .args
            .get(index - 1)
            .is_some_and(|arg| arg.eq_ignore_ascii_case("/C"))
    {
        // The /C argument is already a cmd command string from prepare_task_for_spawn.
        // Quoting it again for venv activation makes cmd see the quotes as literals.
        return quote_cmd_command_arg_for_outer_shell(arg, shell_kind).map(Cow::Owned);
    }

    shell_kind.try_quote(arg)
}

fn quote_cmd_command_arg_for_outer_shell(arg: &str, shell_kind: ShellKind) -> Option<String> {
    match shell_kind {
        ShellKind::PowerShell | ShellKind::Pwsh => Some(format!("'{}'", arg.replace('\'', "''"))),
        ShellKind::Cmd => Some(arg.to_string()),
        ShellKind::Posix
        | ShellKind::Csh
        | ShellKind::Tcsh
        | ShellKind::Fish
        | ShellKind::Nushell
        | ShellKind::Rc
        | ShellKind::Xonsh
        | ShellKind::Elvish => shell_kind.try_quote(arg).map(Cow::into_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SERVER_BINARY: &str = ".zed_server/zed-remote-server-dev-build";

    fn server_binary() -> Arc<RelPath> {
        RelPath::new(Path::new(SERVER_BINARY), PathStyle::Unix)
            .expect("a valid relative path")
            .into_arc()
    }

    fn persistence(enabled: bool) -> RemotePersistenceSettings {
        RemotePersistenceSettings {
            enabled,
            detached_timeout_hours: 24,
        }
    }

    fn launch(resume_session_id: Option<&str>) -> PersistentSessionLaunch {
        PersistentSessionLaunch::new(
            server_binary(),
            resume_session_id.map(ToOwned::to_owned),
            &persistence(true),
        )
    }

    /// The command line `build_command_posix` makes of a program and its
    /// arguments, read back the way the remote login shell reads it.
    fn as_posix_shell_reads_it(program: &str, args: &[String]) -> Vec<String> {
        let shell_kind = ShellKind::Posix;
        let mut command_line = shell_kind
            .try_quote_prefix_aware(program)
            .expect("the program quotes")
            .into_owned();
        for arg in args {
            command_line.push(' ');
            command_line.push_str(&shell_kind.try_quote(arg).expect("the argument quotes"));
        }
        shell_kind
            .split(&command_line)
            .expect("the command line splits back")
    }

    #[test]
    fn only_ssh_shells_on_posix_hosts_get_a_persistent_session() {
        let binary = server_binary();
        assert_eq!(
            persistent_session_binary(
                true,
                PathStyle::Unix,
                Some(binary.clone()),
                &persistence(true),
                None
            ),
            Some(binary.clone())
        );
        // WSL and Docker connections, which do not drop with the network.
        assert_eq!(
            persistent_session_binary(
                false,
                PathStyle::Unix,
                Some(binary.clone()),
                &persistence(true),
                None
            ),
            None
        );
        assert_eq!(
            persistent_session_binary(
                true,
                PathStyle::Windows,
                Some(binary.clone()),
                &persistence(true),
                None
            ),
            None,
            "a Windows host has no holder"
        );
        assert_eq!(
            persistent_session_binary(
                true,
                PathStyle::Unix,
                Some(binary.clone()),
                &persistence(false),
                None
            ),
            None,
            "turned off in the settings"
        );
        assert_eq!(
            persistent_session_binary(true, PathStyle::Unix, None, &persistence(true), None),
            None,
            "no server binary installed yet"
        );
        assert_eq!(
            persistent_session_binary(
                true,
                PathStyle::Unix,
                Some(binary.clone()),
                &persistence(true),
                Some(binary.as_ref())
            ),
            None,
            "a binary that refused the subcommand once is not asked again"
        );
        let other_binary = RelPath::new(
            Path::new(".zed_server/zed-remote-server-dev-newer"),
            PathStyle::Unix,
        )
        .expect("a valid relative path")
        .into_arc();
        assert_eq!(
            persistent_session_binary(
                true,
                PathStyle::Unix,
                Some(other_binary.clone()),
                &persistence(true),
                Some(binary.as_ref())
            ),
            Some(other_binary),
            "a newer binary gets asked again"
        );
    }

    fn ssh_host(binary: Option<Arc<RelPath>>) -> RemoteSessionHost {
        RemoteSessionHost {
            is_ssh: true,
            path_style: PathStyle::Unix,
            binary,
        }
    }

    /// While the connection is being re-established the client knows no server
    /// binary; the shell of a tab opened then starts once it is back, which is
    /// when its session has to be decided.
    #[test]
    fn a_tab_opened_while_reconnecting_gets_its_session_once_the_server_is_back() {
        let request = RemoteSessionRequest::Attach {
            resume_session_id: None,
        };
        assert!(
            request
                .clone()
                .launch(ssh_host(None), &persistence(true), None)
                .is_none(),
            "nothing to attach through while reconnecting"
        );

        let launch = request
            .launch(ssh_host(Some(server_binary())), &persistence(true), None)
            .expect("the server binary is known again");
        assert!(!resumes_session(Some(&launch)));
        let PersistentShellCommand::Attached {
            program, session, ..
        } = persistent_session_shell_command(
            Some(&launch),
            Some("/bin/bash".to_string()),
            None,
            None,
            Vec::new(),
        )
        else {
            panic!("the shell should start through the session");
        };
        assert_eq!(program, SERVER_BINARY);
        // What the terminal reports as its session id, and its tab stores.
        assert_eq!(session, launch.session);
        assert!(is_valid_session_id(&session.id));

        assert!(
            RemoteSessionRequest::Off
                .launch(ssh_host(Some(server_binary())), &persistence(true), None)
                .is_none(),
            "a shell outside a tab never gets a session"
        );
    }

    #[test]
    fn only_a_shell_resumed_in_its_session_goes_without_the_toolchain_activation() {
        let session_id = "0f8e5d7c-2a4b-4c6d-8e9f-001122334455";
        let request = RemoteSessionRequest::Attach {
            resume_session_id: Some(session_id.to_string()),
        };

        let resumed = request
            .clone()
            .launch(ssh_host(Some(server_binary())), &persistence(true), None)
            .expect("the server binary is known");
        assert_eq!(resumed.session.id, session_id);
        assert!(resumes_session(Some(&resumed)));

        // Without a binary to attach through, the id cannot be resumed: the
        // shell is a new one, which gets the activation like any other.
        let without_binary = request.launch(ssh_host(None), &persistence(true), None);
        assert!(without_binary.is_none());
        assert!(!resumes_session(without_binary.as_ref()));

        assert!(!resumes_session(Some(&launch(None))));
    }

    #[test]
    fn a_new_session_gets_a_fresh_id_and_a_resumed_one_keeps_its_own() {
        let first = launch(None);
        let second = launch(None);
        assert!(!first.session.resume);
        assert!(is_valid_session_id(&first.session.id));
        assert_ne!(first.session.id, second.session.id);

        let resumed = launch(Some(&first.session.id));
        assert!(resumed.session.resume);
        assert_eq!(resumed.session.id, first.session.id);
        assert_ne!(
            resumed.session.nonce, first.session.nonce,
            "every attach has a nonce of its own"
        );
        assert!(
            resumed
                .session
                .nonce
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
                && (8..=64).contains(&resumed.session.nonce.len())
        );

        assert_eq!(first.detached_timeout_secs, 24 * 60 * 60);
    }

    /// A row of the terminals table holds whatever was written into it; an id
    /// the server's `[A-Za-z0-9_-]{1,64}` refuses would fail the attach with
    /// its usage error, which reads as a server without sessions.
    #[test]
    fn a_resume_id_the_server_would_refuse_starts_a_new_session() {
        let longest = "a".repeat(64);
        let resumed = launch(Some(&longest));
        assert!(resumed.session.resume, "64 characters are still an id");
        assert_eq!(resumed.session.id, longest);

        for refused in [
            "a".repeat(65),
            String::new(),
            "not a valid id; rm -rf ~".to_string(),
            "../../etc".to_string(),
            "セッション".to_string(),
            "a b".to_string(),
        ] {
            let launch = launch(Some(&refused));
            assert!(!launch.session.resume, "{refused:?} is not resumed");
            assert_ne!(launch.session.id, refused);
            assert!(is_valid_session_id(&launch.session.id));
            let (_, args) = persistent_session_command(
                SERVER_BINARY,
                &launch.session,
                launch.detached_timeout_secs,
                "/bin/sh".to_string(),
                Vec::new(),
            );
            assert!(
                !args.contains(&refused),
                "{refused:?} never reaches the command line"
            );
            assert!(!args.contains(&"--resume".to_string()));
        }
    }

    #[test]
    fn the_login_shell_is_started_through_the_session() {
        let launch = launch(Some("0f8e5d7c-2a4b-4c6d-8e9f-001122334455"));
        let PersistentShellCommand::Attached {
            program,
            args,
            session,
        } = persistent_session_shell_command(
            Some(&launch),
            Some("/bin/bash".to_string()),
            None,
            None,
            Vec::new(),
        )
        else {
            panic!("a login shell with a session should be wrapped");
        };
        assert_eq!(session, launch.session);
        assert_eq!(program, SERVER_BINARY);
        let expected = vec![
            "terminal-session".to_string(),
            "attach".to_string(),
            "--id".to_string(),
            "0f8e5d7c-2a4b-4c6d-8e9f-001122334455".to_string(),
            "--nonce".to_string(),
            launch.session.nonce,
            "--resume".to_string(),
            "--detached-timeout-secs".to_string(),
            "86400".to_string(),
            "--".to_string(),
            "/bin/bash".to_string(),
            "-l".to_string(),
        ];
        assert_eq!(args, expected);

        let mut read_back = vec![SERVER_BINARY.to_string()];
        read_back.extend(expected);
        assert_eq!(as_posix_shell_reads_it(&program, &args), read_back);
    }

    #[test]
    fn the_cwd_fallback_wrapper_runs_inside_the_session() {
        let launch = launch(None);
        let (fallback_program, fallback_args) =
            shell_command_with_cwd_fallback(Path::new("/srv/my app/it's here"), "/usr/bin/fish")
                .expect("the directory quotes");
        let PersistentShellCommand::Attached { program, args, .. } =
            persistent_session_shell_command(
                Some(&launch),
                Some("/usr/bin/fish".to_string()),
                None,
                Some(fallback_program.clone()),
                fallback_args.clone(),
            )
        else {
            panic!("the fallback wrapper should be wrapped");
        };

        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("the shell command follows `--`");
        assert!(!args[..separator].contains(&"--resume".to_string()));
        let mut inner = vec![fallback_program];
        inner.extend(fallback_args);
        assert_eq!(args[separator + 1..].to_vec(), inner);

        // The wrapper's script survives the quoting for the login shell intact,
        // so the `sh -c` inside the session gets exactly the script it would
        // have got without one.
        let read_back = as_posix_shell_reads_it(&program, &args);
        assert_eq!(read_back[separator + 2..].to_vec(), inner);
    }

    #[test]
    fn a_shell_without_a_session_is_left_alone() {
        let launch = launch(None);
        for (persistent_session, remote_shell, working_directory) in [
            (None, Some("/bin/bash".to_string()), None),
            // A directory the fallback wrapper could not take over has to be
            // entered by `ssh`'s own `cd`, after which the binary's
            // home-relative path no longer resolves.
            (
                Some(&launch),
                Some("/bin/bash".to_string()),
                Some(Path::new("/srv")),
            ),
            (Some(&launch), None, None),
        ] {
            let command = persistent_session_shell_command(
                persistent_session,
                remote_shell,
                working_directory,
                None,
                Vec::new(),
            );
            assert!(
                matches!(
                    &command,
                    PersistentShellCommand::Plain { program: None, args } if args.is_empty()
                ),
                "should not be wrapped"
            );
        }
    }

    fn prepared_cmd_task(command_arg: &str) -> SpawnInTerminal {
        SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec!["/S".to_string(), "/C".to_string(), command_arg.to_string()],
            shell: Shell::Program("cmd.exe".to_string()),
            ..SpawnInTerminal::default()
        }
    }

    #[test]
    fn formats_prepared_cmd_task_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_for_cmd_activation() {
        let task = prepared_cmd_task("\"echo Hi there\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::Cmd, "cmd.exe", true),
            "cmd.exe /S /C \"echo Hi there\""
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_shell_args_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cmd.exe".to_string()),
            args: vec![
                "/D".to_string(),
                "/S".to_string(),
                "/C".to_string(),
                "\"echo Hi there\"".to_string(),
            ],
            shell: Shell::WithArguments {
                program: "cmd.exe".to_string(),
                args: vec!["/D".to_string()],
                title_override: None,
            },
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /D /S /C '\"echo Hi there\"'"
        );
    }

    #[test]
    fn formats_prepared_cmd_task_with_single_quote_for_powershell_activation() {
        let task = prepared_cmd_task("\"echo It's fine\"");

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cmd.exe /S /C '\"echo It''s fine\"'"
        );
    }

    #[test]
    fn formats_non_cmd_task_for_activation() {
        let task = SpawnInTerminal {
            command: Some("cargo".to_string()),
            args: vec!["test".to_string(), "some test".to_string()],
            shell: Shell::System,
            ..SpawnInTerminal::default()
        };

        assert_eq!(
            format_task_for_activation(&task, ShellKind::PowerShell, "powershell.exe", true),
            "&cargo test 'some test'"
        );
    }

    #[test]
    fn sandboxed_agent_commands_get_nothing_from_the_terminal() {
        let directory_environment = HashMap::from_iter([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/user".to_string()),
        ]);
        let provided_environment = vec![("CLAUDE_CODE_SSE_PORT".to_string(), "4000".to_string())];
        let settings_environment = HashMap::from_iter([
            ("LD_PRELOAD".to_string(), "/project/evil.so".to_string()),
            ("PATH".to_string(), "/opt/bin".to_string()),
        ]);

        assert_eq!(
            terminal_process_environment(
                TaskSpawnEnvironment::SandboxWrapped,
                directory_environment.clone(),
                provided_environment.clone(),
                settings_environment.clone(),
            ),
            HashMap::default(),
            "the process that starts a sandbox wrapper must not see the project's environment"
        );

        let full = terminal_process_environment(
            TaskSpawnEnvironment::Full,
            directory_environment,
            provided_environment,
            settings_environment,
        );
        assert_eq!(
            full.get("LD_PRELOAD").map(String::as_str),
            Some("/project/evil.so")
        );
        assert_eq!(
            full.get("PATH").map(String::as_str),
            Some("/opt/bin"),
            "`terminal.env` still wins over the directory environment elsewhere"
        );
        assert_eq!(full.get("HOME").map(String::as_str), Some("/home/user"));
        assert_eq!(
            full.get("CLAUDE_CODE_SSE_PORT").map(String::as_str),
            Some("4000")
        );
    }

    #[test]
    fn toolchain_activation_is_never_prepended_to_a_sandboxed_command() {
        assert!(TaskSpawnEnvironment::Full.activates_toolchain(true));
        assert!(!TaskSpawnEnvironment::Full.activates_toolchain(false));
        assert!(!TaskSpawnEnvironment::SandboxWrapped.activates_toolchain(true));
        assert!(!TaskSpawnEnvironment::SandboxWrapped.activates_toolchain(false));
    }

    #[gpui::test]
    async fn sandboxed_spawns_neither_capture_nor_ask_providers(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(util::path!("/root"), serde_json::json!({ "main.rs": "" }))
            .await;
        let project = Project::test(fs, [util::path!("/root").as_ref()], cx).await;
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        cx.update(|cx| {
            register_terminal_environment_provider(
                {
                    let asked = asked.clone();
                    move |_, _| {
                        asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        vec![("PROVIDED".to_string(), "1".to_string())]
                    }
                },
                cx,
            );
        });
        let root: Arc<Path> = Path::new(util::path!("/root")).into();

        let (directory_environment, provided_environment) = project.update(cx, |project, cx| {
            project.terminal_environment_sources(
                TaskSpawnEnvironment::SandboxWrapped,
                "sh",
                Some(root.clone()),
                None,
                cx,
            )
        });
        assert_eq!(
            directory_environment.await,
            None,
            "a sandboxed spawn captures no directory environment"
        );
        assert!(provided_environment.is_empty());
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "and asks no provider"
        );

        let (directory_environment, provided_environment) = project.update(cx, |project, cx| {
            project.terminal_environment_sources(
                TaskSpawnEnvironment::Full,
                "sh",
                Some(root),
                None,
                cx,
            )
        });
        assert!(directory_environment.await.is_some());
        assert_eq!(
            provided_environment,
            vec![("PROVIDED".to_string(), "1".to_string())]
        );
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
