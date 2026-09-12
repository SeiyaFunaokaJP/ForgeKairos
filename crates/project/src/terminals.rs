use anyhow::Result;
use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};

use async_channel::bounded;
use futures::{FutureExt, future::Shared};
use itertools::Itertools as _;
use language::LanguageName;
use remote::{Interactive, RemoteClient};
use settings::{Settings, SettingsLocation};
use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
};
use task::{Shell, ShellBuilder, ShellKind, SpawnInTerminal};
use terminal::{
    TaskState, TaskStatus, Terminal, TerminalBuilder, insert_zed_terminal_env,
    remote_cwd::{osc7_prompt_command_env, shell_command_with_cwd_fallback},
    terminal_settings::TerminalSettings,
};
use util::{
    command::new_std_command, get_default_system_shell, get_system_shell, maybe, rel_path::RelPath,
};

use crate::{Project, ProjectPath};

pub struct Terminals {
    pub(crate) local_handles: Vec<WeakEntity<terminal::Terminal>>,
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

        let (completion_tx, completion_rx) = bounded(1);

        let local_path = if is_via_remote { None } else { path.clone() };
        let task_state = Some(TaskState {
            spawned_task: spawn_task.clone(),
            status: TaskStatus::Running,
            completion_rx,
        });
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
                                _ => spawn_remote_command(
                                    spawn_task
                                        .command
                                        .as_ref()
                                        .map(|command| (command, &spawn_task.args)),
                                    env,
                                    path,
                                    remote_client,
                                    spawn_environment,
                                    cx,
                                )?,
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
                        task_state,
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        settings.path_hyperlink_timeout_ms,
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        Some(completion_tx),
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
        self.create_terminal_shell_internal(cwd, false, cx)
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
        self.create_terminal_shell_internal(working_directory, true, cx)
    }

    /// Internal method for creating terminal shells.
    /// If force_local is true, creates a local terminal even if the project has a remote client.
    /// This allows "breaking out" to a local shell in remote projects.
    fn create_terminal_shell_internal(
        &mut self,
        cwd: Option<PathBuf>,
        force_local: bool,
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
                    let (shell, env) = {
                        match remote_client {
                            Some(remote_client) => {
                                create_remote_shell(None, env, path, remote_client, cx)?
                            }
                            None => (settings.shell, env),
                        }
                    };
                    anyhow::Ok(TerminalBuilder::new(
                        local_path.map(|path| path.to_path_buf()),
                        None,
                        shell,
                        env,
                        settings.cursor_shape,
                        settings.alternate_scroll,
                        settings.max_scroll_history_lines,
                        settings.path_hyperlink_regexes,
                        settings.path_hyperlink_timeout_ms,
                        is_via_remote,
                        cx.entity_id().as_u64(),
                        None,
                        cx,
                        activation_script,
                        path_style,
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
            if let Some(remote_cwd) = terminal.read(cx).remote_working_directory() {
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
    spawn_remote_command(
        spawn_command,
        env,
        working_directory,
        remote_client,
        TaskSpawnEnvironment::Full,
        cx,
    )
}

fn spawn_remote_command(
    spawn_command: Option<(&String, &Vec<String>)>,
    mut env: HashMap<String, String>,
    mut working_directory: Option<Arc<Path>>,
    remote_client: Entity<RemoteClient>,
    spawn_environment: TaskSpawnEnvironment,
    cx: &mut App,
) -> Result<(Shell, HashMap<String, String>)> {
    if spawn_environment == TaskSpawnEnvironment::Full {
        insert_zed_terminal_env(&mut env, &release_channel::AppVersion::global(cx));
    }

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
                .filter(|_| !remote_client.read(cx).path_style().is_windows())
                .zip(working_directory.as_deref())
                .and_then(|(remote_shell, directory)| {
                    shell_command_with_cwd_fallback(directory, &remote_shell)
                });
            match cwd_fallback {
                Some((program, args)) => {
                    working_directory = None;
                    (Some(program), args)
                }
                None => (None, Vec::new()),
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
    ))
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
