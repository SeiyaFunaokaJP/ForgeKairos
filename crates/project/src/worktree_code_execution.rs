//! ADR-F50: whether a worktree's own code may run.
//!
//! An AI agent's write into a worktree advances that worktree's generation
//! before the write lands, so that the save it triggers (format on save, a
//! language server's `didSave`) already finds the worktree paused. While a
//! worktree is paused, language servers run in their paused mode or not at
//! all, and formatters that could run the worktree's code are skipped. Only
//! the user, from the resume dialog, confirms the generation again.
//!
//! A write pauses every open worktree on the same host that holds the written
//! path or lies inside it, however either was spelled, whichever project or
//! window it belongs to. The IDE decides; a remote project's host follows
//! through `SetWorktreeCodeExecution`.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map},
    ffi::OsString,
    mem,
    path::{Component, Path, PathBuf},
    rc::Rc,
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{Context as _, Result};
use client::{TypedEnvelope, proto};
use collections::HashMap;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, SharedString,
    Subscription, Task, TaskExt as _, WeakEntity,
};
use lsp::LanguageServerName;
use parking_lot::Mutex;
use remote::{ConnectionState, RemoteClient};
use rpc::AnyProtoClient;
use serde::{Deserialize, Serialize};
use settings::{SettingsContent, SettingsStore, UserSettingsContentExt as _};
use sha2::{Digest as _, Sha256};
use util::{ResultExt as _, paths::SanitizedPath};
use worktree::{Snapshot, UpdatedEntriesSet, Worktree, WorktreeId};

use crate::{
    lsp_store::dev_tools,
    project_launch_approvals::{LaunchHost, program_stem},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

const DB_NAMESPACE: &str = "worktree_code_execution";

/// The host of this machine's worktrees.
pub const THIS_MACHINE: &str = "this-machine";

/// A remote server from before ADR-F50 never answers
/// `SetWorktreeCodeExecution`.
pub const REMOTE_ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(15);

/// Files that decide what a restricted rust-analyzer runs. Cargo's
/// configuration and rustup's toolchain file can name programs
/// (`build.rustc-wrapper`, a toolchain path), `rust-project.json` names a
/// sysroot and build commands, and `rust-analyzer.toml` holds the server's
/// own settings. A language server's `unchanged_files` must be among them.
const ON_DISK_GUARD_FILES: &[&str] = &[
    ".cargo/config",
    ".cargo/config.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rust-analyzer.toml",
    "rust-project.json",
    ".rust-project.json",
];

/// Project settings feed every language server's options, and Zed loads them
/// from the files its scan finds.
const PROJECT_SETTINGS_FILE: &str = ".zed/settings.json";

/// A change to one of these can change what the guard files hold or where
/// they are looked for: they are guard files, or a manifest whose directory
/// is looked into.
const GUARD_FILE_TRIGGERS: &[&str] = &[
    ".cargo/config",
    ".cargo/config.toml",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rust-analyzer.toml",
    "rust-project.json",
    ".rust-project.json",
    PROJECT_SETTINGS_FILE,
    "Cargo.toml",
];

/// Formatters that may run while a worktree's code is paused, when their
/// executable is outside the worktree: native programs that read only
/// declarative configuration from the project, and load no plugins, scripts,
/// interpreters or toolchains from it. Everything else waits for the resume:
/// package runners (`npx`, `bunx`, `poetry run`, `bundle exec`, `mix`),
/// interpreters, `rustfmt` and `cargo` (rustup picks the project's
/// toolchain), prettier and its plugins, rubocop, `dotnet format`
/// (analyzers), `dprint` (process plugins), php-cs-fixer (a PHP config).
pub const FORMATTERS_ALLOWED_WHILE_PAUSED: &[&str] = &[
    // `.clang-format` is YAML.
    "clang-format",
    // Reads no configuration.
    "gofmt",
    // Reads `.editorconfig`.
    "shfmt",
    // `stylua.toml` and `.editorconfig`.
    "stylua",
    // `taplo.toml`; the schemas it fetches are data.
    "taplo",
    // `pyproject.toml` or `ruff.toml`; a native program without Python
    // plugins.
    "ruff",
    // `biome.json`; its plugins are GritQL patterns, not code.
    "biome",
    // Neither reads any configuration.
    "nixfmt",
    "alejandra",
];

/// Each guard file's worktree-relative path (case-folded where the filesystem
/// ignores case) and the SHA-256 of what it holds, `None` when unreadable.
type GuardFiles = BTreeMap<String, Option<String>>;

/// Whether a worktree's own code may run now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeCodeState {
    /// Not known yet: whether the worktree was left paused, or what the IDE
    /// decided, is still to come. Nothing that could run its code starts in
    /// the meantime.
    Unknown,
    Allowed,
    /// An agent wrote into the worktree since the user last confirmed it.
    Paused,
}

/// How a language server of a paused worktree runs, for the resume dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausedLanguageServer {
    Stopped,
    Restricted,
}

/// Whether a remote project's host follows the pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteHostPause {
    /// It has not acknowledged yet.
    Unknown,
    Following,
    /// A server from before ADR-F50: its language servers and formatters keep
    /// running.
    NotFollowing,
}

/// What the resume dialog shows about a paused worktree.
#[derive(Clone, Debug, PartialEq)]
pub struct PausedWorktreeDetails {
    pub language_servers: BTreeMap<LanguageServerName, PausedLanguageServer>,
    pub skipped_formatters: BTreeSet<String>,
    /// A process that can write the worktree still runs, and the next change
    /// it makes pauses the worktree again.
    pub sandboxed_writer_running: bool,
    /// One of them is an external agent's turn, which ends when the agent
    /// answers or is stopped.
    pub agent_turn_running: bool,
    /// `None` for a worktree on this machine.
    pub remote_host: Option<RemoteHostPause>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCodeExecutionEvent {
    Paused(WorktreeId),
    Resumed(WorktreeId),
    /// The paused worktree's guard files may have changed where no change
    /// reaches the language servers (a writer ended, an agent's write landed,
    /// the IDE reported one), so they are to be checked on disk.
    CheckGuardFiles(WorktreeId),
    /// The extension language servers the user lets run while paused changed.
    LanguageServersAllowedWhilePausedChanged,
    /// What the resume dialog shows about the worktree changed.
    DetailsChanged(WorktreeId),
}

/// What a resume came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeOutcome {
    Resumed,
    /// An agent wrote again after the dialog showed the worktree.
    WrittenSinceShown,
    /// A process that can write the worktree still runs.
    WriterRunning,
    NotPaused,
}

/// Where an agent's writes can land on a host.
#[derive(Clone, Debug)]
pub enum AgentWriteScope {
    /// These paths and whatever is inside them.
    Paths(Vec<PathBuf>),
    /// Anywhere: every worktree on the host.
    Anywhere,
}

/// What kind of process a writer is, for the resume dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentWriterKind {
    /// A command an agent runs.
    Command,
    /// An external agent's turn: the agent's own process writes.
    AgentTurn,
}

pub(crate) struct WriterRegistration {
    kind: AgentWriterKind,
    /// How the pause states the writer is registered with learn that it ended.
    ended: Mutex<Vec<mpsc::UnboundedSender<()>>>,
}

/// Keeps a process registered as a writer of the worktrees it can write until
/// it is dropped, which has to happen when the process exits.
pub struct SandboxedWriterGuard {
    registration: Option<Arc<WriterRegistration>>,
}

impl SandboxedWriterGuard {
    pub(crate) fn registration(&self) -> Option<Weak<WriterRegistration>> {
        self.registration.as_ref().map(Arc::downgrade)
    }
}

impl Default for SandboxedWriterGuard {
    /// The guard of a process that can write no worktree.
    fn default() -> Self {
        Self { registration: None }
    }
}

impl Drop for SandboxedWriterGuard {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };
        let ended = mem::take(&mut *registration.ended.lock());
        // Gone before the pause states hear of it, so that they find it ended.
        drop(registration);
        for ended in ended {
            if ended.unbounded_send(()).is_err() {
                log::debug!("a worktree code execution state is gone; no writer to forget");
            }
        }
    }
}

/// ADR-F49 decision 18 (C): one process an external agent's answer left
/// running inside its sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftoverSandboxProcess {
    /// Its pid on the host that runs the sandbox, which is the only name the
    /// user can look it up by there.
    pub pid: u32,
    /// Its command line, as the trusted side outside the sandbox read it.
    /// Chosen by the process itself, so the dialog escapes it again.
    pub command: SharedString,
    /// How long it had been running when the report was taken.
    pub running_for: Duration,
}

/// What one agent left running, as the resume dialog lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftoverSandboxProcesses {
    pub id: LeftoverSandboxProcessesId,
    /// The agent whose answer left them, for the user to know which panel to
    /// look at.
    pub agent: SharedString,
    pub processes: Vec<LeftoverSandboxProcess>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeftoverSandboxProcessesId(u64);

struct LeftoverReportToken;

/// Keeps a report listed for as long as it lives. The agent side holds one
/// per sandboxed session; dropping it (the session closed, which also stops
/// the sandbox) takes the report away without needing a context.
pub struct LeftoverSandboxProcessesHandle {
    id: LeftoverSandboxProcessesId,
    _token: Arc<LeftoverReportToken>,
}

impl LeftoverSandboxProcessesHandle {
    pub fn id(&self) -> LeftoverSandboxProcessesId {
        self.id
    }
}

struct LeftoverReport {
    id: LeftoverSandboxProcessesId,
    token: Weak<LeftoverReportToken>,
    host: String,
    scope: AgentWriteScope,
    agent: SharedString,
    processes: Vec<LeftoverSandboxProcess>,
    /// Stops exactly the processes listed above, and nothing else — not the
    /// agent itself, which the user stops from its own panel.
    stop: Rc<dyn Fn(&mut App)>,
}

/// ADR-F49 decision 18 (C): what external agents have left running in their
/// sandboxes, for the resume dialog to list and to stop. One per application,
/// because a pause is about a host and a path, not about a window.
#[derive(Default)]
pub struct LeftoverSandboxProcessReports {
    next_id: u64,
    reports: Vec<LeftoverReport>,
}

struct GlobalLeftoverSandboxProcessReports(Entity<LeftoverSandboxProcessReports>);

impl Global for GlobalLeftoverSandboxProcessReports {}

impl EventEmitter<()> for LeftoverSandboxProcessReports {}

impl LeftoverSandboxProcessReports {
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalLeftoverSandboxProcessReports>() {
            return global.0.clone();
        }
        let reports = cx.new(|_| Self::default());
        cx.set_global(GlobalLeftoverSandboxProcessReports(reports.clone()));
        reports
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalLeftoverSandboxProcessReports>()
            .map(|global| global.0.clone())
    }

    /// Starts listing what an agent has left running within `scope` on `host`.
    /// The list is empty until [`Self::report`] fills it, and is gone once the
    /// returned handle is.
    pub fn publish(
        host: &str,
        scope: AgentWriteScope,
        agent: SharedString,
        stop: Rc<dyn Fn(&mut App)>,
        cx: &mut App,
    ) -> LeftoverSandboxProcessesHandle {
        let token = Arc::new(LeftoverReportToken);
        let reports = Self::global(cx);
        let id = reports.update(cx, |reports, cx| {
            reports.forget_dropped();
            reports.next_id += 1;
            let id = LeftoverSandboxProcessesId(reports.next_id);
            reports.reports.push(LeftoverReport {
                id,
                token: Arc::downgrade(&token),
                host: host.to_string(),
                scope,
                agent,
                processes: Vec::new(),
                stop,
            });
            cx.notify();
            id
        });
        LeftoverSandboxProcessesHandle { id, _token: token }
    }

    /// Replaces what `id` lists.
    pub fn report(
        id: LeftoverSandboxProcessesId,
        processes: Vec<LeftoverSandboxProcess>,
        cx: &mut App,
    ) {
        let Some(reports) = Self::try_global(cx) else {
            return;
        };
        reports.update(cx, |reports, cx| {
            reports.forget_dropped();
            let Some(report) = reports.reports.iter_mut().find(|report| report.id == id) else {
                return;
            };
            if report.processes == processes {
                return;
            }
            report.processes = processes;
            cx.notify();
        });
    }

    /// Asks the agent that left them to stop exactly the processes `id`
    /// lists. The report goes away on its own once they are gone.
    pub fn stop(id: LeftoverSandboxProcessesId, cx: &mut App) {
        let Some(reports) = Self::try_global(cx) else {
            return;
        };
        let stop = reports.read(cx).reports.iter().find_map(|report| {
            (report.id == id && report.token.strong_count() > 0).then(|| report.stop.clone())
        });
        if let Some(stop) = stop {
            stop(cx);
        }
    }

    fn forget_dropped(&mut self) {
        self.reports
            .retain(|report| report.token.strong_count() > 0);
    }

    fn listed(
        &self,
        covers: impl Fn(&str, &AgentWriteScope) -> bool,
    ) -> Vec<LeftoverSandboxProcesses> {
        self.reports
            .iter()
            .filter(|report| {
                report.token.strong_count() > 0
                    && !report.processes.is_empty()
                    && covers(&report.host, &report.scope)
            })
            .map(|report| LeftoverSandboxProcesses {
                id: report.id,
                agent: report.agent.clone(),
                processes: report.processes.clone(),
            })
            .collect()
    }
}

/// Keeps an external agent's session known while it lasts: what changes
/// between its turns can come from what it left running, and is not taken for
/// the user's own change.
#[derive(Default)]
pub struct ExternalAgentSessionGuard {
    _token: Option<Arc<ExternalAgentSessionToken>>,
}

struct ExternalAgentSessionToken;

enum Role {
    /// This IDE's local project: its language servers and formatters follow
    /// this state directly.
    Local { fs: Arc<dyn Fs> },
    /// This IDE's remote project: the remote host hears this state.
    RemoteHost {
        client: AnyProtoClient,
        project_id: u64,
        host_identity: String,
        connected: bool,
        host_follows: RemoteHostPause,
    },
    /// The remote server, which follows what the IDE of the session says.
    Headless { fs: Arc<dyn Fs> },
    /// A collaborator's project, which this fork does not build.
    Inert,
}

impl Role {
    fn decides(&self) -> bool {
        matches!(self, Self::Local { .. } | Self::RemoteHost { .. })
    }

    fn fs(&self) -> Option<&Arc<dyn Fs>> {
        match self {
            Self::Local { fs } | Self::Headless { fs } => Some(fs),
            Self::RemoteHost { .. } | Self::Inert => None,
        }
    }

    fn host_key(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => Some(THIS_MACHINE),
            Self::RemoteHost { host_identity, .. } => Some(host_identity.as_str()),
            Self::Headless { .. } | Self::Inert => None,
        }
    }
}

enum StoredPauses {
    Loading,
    Loaded(HashMap<String, StoredPause>),
    /// No database: nothing is kept, and nothing was left paused.
    Unavailable,
    /// What was kept is lost or cannot be read (a database that does not
    /// persist, a read error), so any worktree could have been left paused.
    Lost,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredPause {
    #[serde(default)]
    confirmed_guard_files: Option<GuardFiles>,
}

/// Where a worktree's guard files are looked for (see
/// [`current_guard_files`]), kept between checks until a change can move
/// them.
#[derive(Default)]
struct GuardLayout {
    /// Worktree-relative directories (Unix separators) whose guard files are
    /// looked up on disk.
    directories: BTreeSet<String>,
    /// The guard files and project settings files the scan found.
    scanned: Vec<String>,
}

struct WorktreeCode {
    abs_path: Arc<Path>,
    /// The root with links, junctions and mapped drives resolved, on this
    /// machine; `None` until resolved.
    canonical_root: Option<PathBuf>,
    /// Where the pause is kept across restarts: the host and the root's
    /// canonical path. `None` until the canonical path is known.
    stored_key: Option<String>,
    generation: u64,
    confirmed_generation: u64,
    known: bool,
    writers: Vec<Weak<WriterRegistration>>,
    confirmed_guard_files: Option<GuardFiles>,
    paused_language_servers: BTreeMap<LanguageServerName, PausedLanguageServer>,
    skipped_formatters: BTreeSet<String>,
    formatter_skips: u64,
    published: WorktreeCodeState,
    state_sender: watch::Sender<WorktreeCodeState>,
    /// The last change set seen: the language server store hands it over
    /// first, and the worktree store again afterwards.
    last_observed_changes: Option<UpdatedEntriesSet>,
    guard_layout: Option<Arc<GuardLayout>>,
    guard_layout_version: u64,
    _hash_guard_files: Task<()>,
    _resolve_root: Task<()>,
}

impl WorktreeCode {
    fn new(abs_path: Arc<Path>) -> Self {
        let (state_sender, _) = watch::channel(WorktreeCodeState::Unknown);
        Self {
            abs_path,
            canonical_root: None,
            stored_key: None,
            generation: 0,
            confirmed_generation: 0,
            known: false,
            writers: Vec::new(),
            confirmed_guard_files: None,
            paused_language_servers: BTreeMap::new(),
            skipped_formatters: BTreeSet::new(),
            formatter_skips: 0,
            published: WorktreeCodeState::Unknown,
            state_sender,
            last_observed_changes: None,
            guard_layout: None,
            guard_layout_version: 0,
            _hash_guard_files: Task::ready(()),
            _resolve_root: Task::ready(()),
        }
    }

    fn state(&self) -> WorktreeCodeState {
        if !self.known {
            WorktreeCodeState::Unknown
        } else if self.generation == self.confirmed_generation {
            WorktreeCodeState::Allowed
        } else {
            WorktreeCodeState::Paused
        }
    }

    fn writer_running(&self) -> bool {
        self.writers.iter().any(|writer| writer.strong_count() > 0)
    }

    fn agent_turn_running(&self) -> bool {
        self.writers
            .iter()
            .filter_map(Weak::upgrade)
            .any(|writer| writer.kind == AgentWriterKind::AgentTurn)
    }

    /// The root as opened and as resolved, either of which a write path can
    /// be spelled against.
    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.abs_path.as_ref()).chain(self.canonical_root.as_deref())
    }

    fn publish(&mut self) {
        let state = self.state();
        if state == self.published {
            return;
        }
        self.published = state;
        if self.state_sender.send(state).is_err() {
            log::debug!("no language server waits on {:?}", self.abs_path);
        }
    }
}

/// Reads an ADR-F50 setting from the user's own settings file only: project
/// settings can be written by the very agent the pause is about, and the IDE
/// never merges a remote host's settings.
fn from_user_settings<T>(cx: &App, read: impl Fn(&SettingsContent) -> Option<T>) -> Option<T> {
    let user = cx.try_global::<SettingsStore>()?.raw_user_settings()?;
    [
        Some(user.content.as_ref()),
        user.for_release_channel(),
        user.for_os(),
    ]
    .into_iter()
    .flatten()
    .filter_map(read)
    .next_back()
}

pub fn pause_after_agent_writes_enabled(cx: &App) -> bool {
    from_user_settings(cx, |content| {
        content
            .agent
            .as_ref()?
            .pause_worktree_code_after_agent_writes
    })
    .unwrap_or(true)
}

/// Extension language servers the user lets run while their worktree is
/// paused.
fn language_servers_allowed_in_user_settings(cx: &App) -> Vec<String> {
    from_user_settings(cx, |content| {
        content
            .agent
            .as_ref()?
            .language_servers_allowed_while_paused
            .clone()
    })
    .unwrap_or_default()
}

/// Makes stored pauses count as lost, as when the database does not persist.
#[cfg(any(test, feature = "test-support"))]
pub struct StoredPausesAreNotKeptForTests;

#[cfg(any(test, feature = "test-support"))]
impl Global for StoredPausesAreNotKeptForTests {}

/// Makes the remote server leave `SetWorktreeCodeExecution` unanswered and
/// unapplied, like a server from before ADR-F50.
#[cfg(any(test, feature = "test-support"))]
pub struct IgnoreWorktreeCodeExecutionForTests;

#[cfg(any(test, feature = "test-support"))]
impl Global for IgnoreWorktreeCodeExecutionForTests {}

/// Holds each check of guard files made before forwarding, once it has read
/// the files, until the value turns true: what lands meanwhile is what such
/// a check cannot have seen.
#[cfg(any(test, feature = "test-support"))]
pub struct GuardChecksWaitForTests(pub watch::Receiver<bool>);

#[cfg(any(test, feature = "test-support"))]
impl Global for GuardChecksWaitForTests {}

/// See [`WorktreeCodeExecution::guard_check_mark`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GuardCheckMark {
    generation: u64,
    layout_version: u64,
}

#[cfg_attr(not(any(test, feature = "test-support")), allow(unused_variables))]
fn stored_pauses_are_kept(cx: &App) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    if cx.has_global::<StoredPausesAreNotKeptForTests>() {
        return false;
    }
    db::app_database_is_persistent()
}

/// Every pause state of the app, so that a write can pause the worktrees of
/// other projects too.
#[derive(Default)]
struct WorktreeCodeExecutions(Vec<WeakEntity<WorktreeCodeExecution>>);

impl Global for WorktreeCodeExecutions {}

pub struct WorktreeCodeExecution {
    role: Role,
    worktree_store: Entity<WorktreeStore>,
    worktrees: HashMap<WorktreeId, WorktreeCode>,
    stored_pauses: StoredPauses,
    /// On the remote server, the roots of worktrees whose IDE decides about
    /// them before anything of theirs runs, until they are added.
    awaiting_ide: Vec<PathBuf>,
    /// On the remote server, what the IDE said about worktrees not added yet.
    announced: HashMap<WorktreeId, (u64, u64)>,
    enabled: bool,
    allowed_language_servers: Vec<String>,
    external_agent_sessions: Vec<Weak<ExternalAgentSessionToken>>,
    writer_ended: mpsc::UnboundedSender<()>,
    _subscriptions: Vec<Subscription>,
    _load_stored_pauses: Task<()>,
    _forget_ended_writers: Task<()>,
}

impl EventEmitter<WorktreeCodeExecutionEvent> for WorktreeCodeExecution {}

impl WorktreeCodeExecution {
    pub fn local(
        worktree_store: &Entity<WorktreeStore>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(Role::Local { fs }, worktree_store, cx)
    }

    pub fn remote_host(
        worktree_store: &Entity<WorktreeStore>,
        remote_client: &Entity<RemoteClient>,
        client: AnyProtoClient,
        project_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let (host_identity, connected) = {
            let remote_client = remote_client.read(cx);
            let host = LaunchHost::from_remote_connection(&remote_client.connection_options());
            (
                host.identity().unwrap_or_default().to_string(),
                remote_client.connection_state() == ConnectionState::Connected,
            )
        };
        let mut this = Self::new(
            Role::RemoteHost {
                client,
                project_id,
                host_identity,
                connected,
                host_follows: RemoteHostPause::Unknown,
            },
            worktree_store,
            cx,
        );
        this._subscriptions
            .push(cx.observe(remote_client, Self::on_connection_changed));
        this
    }

    pub fn headless(
        worktree_store: &Entity<WorktreeStore>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(Role::Headless { fs }, worktree_store, cx)
    }

    pub fn inert(worktree_store: &Entity<WorktreeStore>, cx: &mut Context<Self>) -> Self {
        Self::new(Role::Inert, worktree_store, cx)
    }

    /// For the remote server: the IDE of the session says what each
    /// worktree's code may do.
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_set_worktree_code_execution);
    }

    fn new(role: Role, worktree_store: &Entity<WorktreeStore>, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![cx.subscribe(worktree_store, Self::on_worktree_store_event)];
        let decides = role.decides();
        if decides {
            subscriptions.push(cx.observe_global::<SettingsStore>(Self::on_settings_changed));
        }
        let (stored_pauses, load_stored_pauses) = if !decides {
            (StoredPauses::Unavailable, Task::ready(()))
        } else {
            match key_value_store(cx) {
                Err(error) => {
                    log::warn!("worktree pauses are not kept across restarts: {error:#}");
                    (StoredPauses::Unavailable, Task::ready(()))
                }
                Ok(_) if !stored_pauses_are_kept(cx) => {
                    log::warn!(
                        "the database does not persist, so every worktree starts with its code paused"
                    );
                    (StoredPauses::Lost, Task::ready(()))
                }
                Ok(store) => {
                    let rows =
                        cx.background_spawn(async move { store.scoped(DB_NAMESPACE).read_all() });
                    let load = cx.spawn(async move |this, cx| {
                        let rows = rows.await;
                        this.update(cx, |this, cx| this.finish_loading(rows, cx))
                            .log_err();
                    });
                    (StoredPauses::Loading, load)
                }
            }
        };
        let (writer_ended, mut writers_ended) = mpsc::unbounded::<()>();
        let forget_ended_writers = cx.spawn(async move |this, cx| {
            while writers_ended.next().await.is_some() {
                if this
                    .update(cx, |this, cx| this.forget_ended_writers(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let this_execution = cx.weak_entity();
        cx.default_global::<WorktreeCodeExecutions>()
            .0
            .push(this_execution);

        let mut this = Self {
            enabled: decides && pause_after_agent_writes_enabled(cx),
            allowed_language_servers: if decides {
                language_servers_allowed_in_user_settings(cx)
            } else {
                Vec::new()
            },
            role,
            worktree_store: worktree_store.clone(),
            worktrees: HashMap::default(),
            stored_pauses,
            awaiting_ide: Vec::new(),
            announced: HashMap::default(),
            external_agent_sessions: Vec::new(),
            writer_ended,
            _subscriptions: subscriptions,
            _load_stored_pauses: load_stored_pauses,
            _forget_ended_writers: forget_ended_writers,
        };
        this.track_new_worktrees(cx);
        this
    }

    fn registered(cx: &mut App) -> Vec<Entity<Self>> {
        let registry = cx.default_global::<WorktreeCodeExecutions>();
        registry.0.retain(|execution| execution.upgrade().is_some());
        registry.0.iter().filter_map(WeakEntity::upgrade).collect()
    }

    /// ADR-F50: an AI agent is about to write within `scope` on `host`. Every
    /// open worktree there that holds a written path, or lies inside one,
    /// stops running its own code first, whichever project it belongs to.
    pub fn note_agent_write(host: &str, scope: &AgentWriteScope, cx: &mut App) {
        for execution in Self::registered(cx) {
            execution.update(cx, |execution, cx| {
                execution.track_new_worktrees(cx);
                for worktree_id in execution.worktrees_within(host, scope) {
                    execution.note_write(worktree_id, cx);
                }
            });
        }
    }

    /// An agent may have written anywhere, on any host: an MCP server can run
    /// on either side and writes wherever it reaches.
    pub fn note_agent_write_on_every_host(cx: &mut App) {
        for execution in Self::registered(cx) {
            execution.update(cx, |execution, cx| {
                let Some(host) = execution.role.host_key().map(str::to_string) else {
                    return;
                };
                execution.track_new_worktrees(cx);
                for worktree_id in execution.worktrees_within(&host, &AgentWriteScope::Anywhere) {
                    execution.note_write(worktree_id, cx);
                }
            });
        }
    }

    /// An agent's write within `scope` on `host` has landed. One where the
    /// worktree's scan does not look reaches no language server as a change,
    /// so the guard files of the paused worktrees it may have reached are
    /// checked on disk.
    pub fn note_agent_write_landed(host: &str, scope: &AgentWriteScope, cx: &mut App) {
        for execution in Self::registered(cx) {
            execution.update(cx, |execution, cx| {
                for worktree_id in execution.worktrees_within(host, scope) {
                    if execution.is_paused(worktree_id) {
                        cx.emit(WorktreeCodeExecutionEvent::CheckGuardFiles(worktree_id));
                    }
                }
            });
        }
    }

    /// An agent's write that could have landed anywhere, on any host, has.
    pub fn note_agent_write_landed_on_every_host(cx: &mut App) {
        for execution in Self::registered(cx) {
            execution.update(cx, |execution, cx| {
                for worktree_id in execution.paused_worktrees() {
                    cx.emit(WorktreeCodeExecutionEvent::CheckGuardFiles(worktree_id));
                }
            });
        }
    }

    /// Registers a process that can write within `scope` on `host` and keeps
    /// running after it starts: until the guard is dropped, every change seen
    /// in a worktree it can write counts as its write. Its start is a write
    /// too, so those worktrees are paused at once.
    pub fn register_agent_writer(
        host: &str,
        scope: &AgentWriteScope,
        kind: AgentWriterKind,
        cx: &mut App,
    ) -> SandboxedWriterGuard {
        let registration = Arc::new(WriterRegistration {
            kind,
            ended: Mutex::default(),
        });
        Self::register_writer_everywhere(host, scope, &registration, cx);
        SandboxedWriterGuard {
            registration: Some(registration),
        }
    }

    /// Has a registered writer cover the worktrees within `scope` as well:
    /// what it can write, spelled as resolved once it registered.
    pub(crate) fn extend_agent_writer(
        host: &str,
        scope: &AgentWriteScope,
        registration: &Weak<WriterRegistration>,
        cx: &mut App,
    ) {
        if let Some(registration) = registration.upgrade() {
            Self::register_writer_everywhere(host, scope, &registration, cx);
        }
    }

    fn register_writer_everywhere(
        host: &str,
        scope: &AgentWriteScope,
        registration: &Arc<WriterRegistration>,
        cx: &mut App,
    ) {
        for execution in Self::registered(cx) {
            let ended = execution.update(cx, |execution, cx| {
                execution.track_new_worktrees(cx);
                execution.register_writer_within(host, scope, registration, cx)
            });
            if let Some(ended) = ended {
                registration.ended.lock().push(ended);
            }
        }
    }

    /// Registers an external agent's session on `host` for as long as the
    /// guard lives.
    pub fn register_external_agent_session(host: &str, cx: &mut App) -> ExternalAgentSessionGuard {
        let token = Arc::new(ExternalAgentSessionToken);
        for execution in Self::registered(cx) {
            execution.update(cx, |execution, _| {
                if execution.role.host_key() == Some(host) {
                    execution
                        .external_agent_sessions
                        .retain(|session| session.strong_count() > 0);
                    execution
                        .external_agent_sessions
                        .push(Arc::downgrade(&token));
                }
            });
        }
        ExternalAgentSessionGuard {
            _token: Some(token),
        }
    }

    /// The host this decides for: [`THIS_MACHINE`], or a remote host's
    /// identity. `None` where agents' writes are not noted.
    pub fn host_key(&self) -> Option<&str> {
        self.role.host_key()
    }

    fn track_new_worktrees(&mut self, cx: &mut Context<Self>) {
        let worktrees = self.worktree_store.read(cx).worktrees().collect::<Vec<_>>();
        for worktree in worktrees {
            self.track(&worktree, cx);
        }
    }

    fn worktrees_within(&self, host: &str, scope: &AgentWriteScope) -> Vec<WorktreeId> {
        if !self.enabled || self.role.host_key() != Some(host) {
            return Vec::new();
        }
        self.worktrees
            .iter()
            .filter(|(_, code)| match scope {
                AgentWriteScope::Anywhere => true,
                AgentWriteScope::Paths(paths) => paths.iter().any(|path| {
                    code.roots()
                        .any(|root| path_is_within(path, root) || path_is_within(root, path))
                }),
            })
            .map(|(worktree_id, _)| *worktree_id)
            .collect()
    }

    fn note_write(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let was_paused = self.is_paused(worktree_id);
        self.advance(worktree_id, cx);
        if was_paused {
            cx.emit(WorktreeCodeExecutionEvent::CheckGuardFiles(worktree_id));
        }
    }

    fn register_writer_within(
        &mut self,
        host: &str,
        scope: &AgentWriteScope,
        registration: &Arc<WriterRegistration>,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::UnboundedSender<()>> {
        let writer = Arc::downgrade(registration);
        let worktree_ids = self
            .worktrees_within(host, scope)
            .into_iter()
            .filter(|worktree_id| {
                self.worktrees.get(worktree_id).is_some_and(|code| {
                    !code
                        .writers
                        .iter()
                        .any(|registered| registered.ptr_eq(&writer))
                })
            })
            .collect::<Vec<_>>();
        if worktree_ids.is_empty() {
            return None;
        }
        for worktree_id in &worktree_ids {
            if let Some(code) = self.worktrees.get_mut(worktree_id) {
                code.writers.retain(|writer| writer.strong_count() > 0);
                code.writers.push(writer.clone());
            }
        }
        for worktree_id in worktree_ids {
            self.advance(worktree_id, cx);
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
        Some(self.writer_ended.clone())
    }

    fn external_agent_session_alive(&self) -> bool {
        self.external_agent_sessions
            .iter()
            .any(|session| session.strong_count() > 0)
    }

    /// Whether the worktree's code may run now. A worktree this does not
    /// know of has no code of its own here.
    pub fn state(&self, worktree_id: WorktreeId) -> WorktreeCodeState {
        self.worktrees
            .get(&worktree_id)
            .map(WorktreeCode::state)
            .unwrap_or(WorktreeCodeState::Allowed)
    }

    pub fn is_paused(&self, worktree_id: WorktreeId) -> bool {
        self.state(worktree_id) == WorktreeCodeState::Paused
    }

    pub fn paused_worktrees(&self) -> Vec<WorktreeId> {
        let mut paused = self
            .worktrees
            .iter()
            .filter(|(_, code)| code.state() == WorktreeCodeState::Paused)
            .map(|(worktree_id, _)| *worktree_id)
            .collect::<Vec<_>>();
        paused.sort();
        paused
    }

    pub fn details(&self, worktree_id: WorktreeId) -> Option<PausedWorktreeDetails> {
        let code = self.worktrees.get(&worktree_id)?;
        Some(PausedWorktreeDetails {
            language_servers: code.paused_language_servers.clone(),
            skipped_formatters: code.skipped_formatters.clone(),
            sandboxed_writer_running: code.writer_running(),
            agent_turn_running: code.agent_turn_running(),
            remote_host: match &self.role {
                Role::RemoteHost { host_follows, .. } => Some(*host_follows),
                Role::Local { .. } | Role::Headless { .. } | Role::Inert => None,
            },
        })
    }

    /// ADR-F49 decision 18 (C): what external agents' answers left running in
    /// the sandboxes that can write this worktree. While any of these run the
    /// worktree stays paused, because the next thing one writes is the
    /// agent's write and not the user's.
    pub fn leftover_sandbox_processes(
        &self,
        worktree_id: WorktreeId,
        cx: &App,
    ) -> Vec<LeftoverSandboxProcesses> {
        let Some(reports) = LeftoverSandboxProcessReports::try_global(cx) else {
            return Vec::new();
        };
        reports
            .read(cx)
            .listed(|host, scope| self.worktrees_within(host, scope).contains(&worktree_id))
    }

    /// How many times agents have written into the worktree, as far as this
    /// knows: what the resume dialog binds the user's decision to.
    pub fn generation(&self, worktree_id: WorktreeId) -> Option<u64> {
        Some(self.worktrees.get(&worktree_id)?.generation)
    }

    /// Whether an extension's language server may run while its worktree is
    /// paused: only those the user lists in their own settings.
    pub fn language_server_allowed_while_paused(&self, name: &str) -> bool {
        self.allowed_language_servers
            .iter()
            .any(|allowed| allowed == name)
    }

    /// What a check of the worktree's guard files is taken against: the
    /// agents' writes noted, and where the files are looked for. A later mark
    /// that differs means that something may have landed after the files
    /// were read.
    pub(crate) fn guard_check_mark(&self, worktree_id: WorktreeId) -> Option<GuardCheckMark> {
        let code = self.worktrees.get(&worktree_id)?;
        Some(GuardCheckMark {
            generation: code.generation,
            layout_version: code.guard_layout_version,
        })
    }

    /// Whether a process that can write the worktree runs: what it writes
    /// where no change shows, no check sees before it ends.
    pub(crate) fn writer_running(&self, worktree_id: WorktreeId) -> bool {
        self.worktrees
            .get(&worktree_id)
            .is_some_and(WorktreeCode::writer_running)
    }

    /// Follows the state of a worktree's code as it changes, for a language
    /// server that waits until it may start. `None` for a worktree that is
    /// gone.
    pub(crate) fn state_receiver(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Option<watch::Receiver<WorktreeCodeState>> {
        if !self.worktrees.contains_key(&worktree_id) {
            let worktree = self
                .worktree_store
                .read(cx)
                .worktree_for_id(worktree_id, cx)?;
            self.track(&worktree, cx);
        }
        Some(self.worktrees.get(&worktree_id)?.state_sender.receiver())
    }

    /// Has the language servers of the worktree that wait to start look
    /// again at whether they may, as when its state changes.
    pub(crate) fn wake_waiting_language_servers(&mut self, worktree_id: WorktreeId) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let state = code.state();
        if code.state_sender.send(state).is_err() {
            log::debug!("no language server waits on {:?}", code.abs_path);
        }
    }

    /// The user reviewed what agents wrote into the worktree up to
    /// `seen_generation` and lets its code run again. The guard files are
    /// hashed for the decision itself, and the decision is taken again once
    /// they are, so that restricted servers are held to what the files were
    /// when the user resumed. Only the resume dialog calls this: nothing an
    /// agent can reach does.
    pub fn resume(
        &mut self,
        worktree_id: WorktreeId,
        seen_generation: u64,
        cx: &mut Context<Self>,
    ) -> Task<ResumeOutcome> {
        let Some(code) = self.worktrees.get(&worktree_id) else {
            return Task::ready(ResumeOutcome::NotPaused);
        };
        if let Some(outcome) = refused_resume(code, seen_generation) {
            return Task::ready(outcome);
        }
        let hashing = self.hash_guard_files_now(worktree_id, cx);
        cx.spawn(async move |this, cx| {
            let hashes = match hashing {
                Some(hashing) => Some(hashing.await),
                None => None,
            };
            this.update(cx, |this, cx| {
                this.finish_resume(worktree_id, seen_generation, hashes, cx)
            })
            .unwrap_or(ResumeOutcome::NotPaused)
        })
    }

    fn finish_resume(
        &mut self,
        worktree_id: WorktreeId,
        seen_generation: u64,
        hashes: Option<GuardFiles>,
        cx: &mut Context<Self>,
    ) -> ResumeOutcome {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return ResumeOutcome::NotPaused;
        };
        if let Some(outcome) = refused_resume(code, seen_generation) {
            return outcome;
        }
        let previous = code.state();
        code.confirmed_generation = code.generation;
        code.confirmed_guard_files = hashes;
        self.settle(worktree_id, previous, false, cx);
        ResumeOutcome::Resumed
    }

    /// Lets go of a pause the user turned off in their settings.
    fn release(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let previous = code.state();
        if previous != WorktreeCodeState::Paused {
            return;
        }
        code.confirmed_generation = code.generation;
        self.settle(worktree_id, previous, true, cx);
    }

    /// A mark to compare with after formatting a buffer of the worktree.
    pub fn formatter_skip_mark(&self, worktree_id: WorktreeId) -> u64 {
        self.worktrees
            .get(&worktree_id)
            .map(|code| code.formatter_skips)
            .unwrap_or_default()
    }

    /// Whether a formatter was skipped for the worktree since `mark`. A remote
    /// host that follows the pause is not asked which, and skips them
    /// whenever the worktree is paused.
    pub fn formatters_skipped_since(&self, worktree_id: WorktreeId, mark: u64) -> bool {
        match &self.role {
            Role::RemoteHost { host_follows, .. } => {
                *host_follows != RemoteHostPause::NotFollowing && self.is_paused(worktree_id)
            }
            Role::Local { .. } | Role::Headless { .. } | Role::Inert => {
                self.formatter_skip_mark(worktree_id) > mark
            }
        }
    }

    pub(crate) fn note_language_server_paused(
        &mut self,
        worktree_id: WorktreeId,
        name: LanguageServerName,
        how: PausedLanguageServer,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if code.state() != WorktreeCodeState::Paused {
            return;
        }
        if code.paused_language_servers.insert(name, how) != Some(how) {
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
    }

    pub(crate) fn note_formatter_skipped(
        &mut self,
        worktree_id: WorktreeId,
        formatter: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        code.formatter_skips += 1;
        if code.skipped_formatters.insert(formatter.to_string()) {
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
    }

    /// Whether every guard file of the worktree, whatever server depends on
    /// it, is on disk now what it was when the user last confirmed the
    /// worktree.
    pub(crate) fn all_guard_files_unchanged(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let unchanged = self.guard_files_unchanged(worktree_id, ON_DISK_GUARD_FILES, cx);
        #[cfg(any(test, feature = "test-support"))]
        if let Some(gate) = cx.try_global::<GuardChecksWaitForTests>() {
            let mut gate = gate.0.clone();
            return cx.spawn(async move |_, _| {
                let unchanged = unchanged.await;
                while !*gate.borrow() {
                    if gate.changed().await.is_err() {
                        break;
                    }
                }
                unchanged
            });
        }
        unchanged
    }

    /// Whether every guard file of the worktree named in `names` (and the
    /// project settings, which feed every server's options) is, on disk now,
    /// what it was when the user last confirmed the worktree.
    pub(crate) fn guard_files_unchanged(
        &mut self,
        worktree_id: WorktreeId,
        names: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        if let Some(unknown) = names
            .iter()
            .find(|name| !ON_DISK_GUARD_FILES.contains(name))
        {
            log::warn!(
                "a language server's paused mode depends on {unknown:?}, which the pause does not keep"
            );
            return Task::ready(false);
        }
        let Some(fs) = self.role.fs().cloned() else {
            return Task::ready(false);
        };
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return Task::ready(false);
        };
        let Some(code) = self.worktrees.get(&worktree_id) else {
            return Task::ready(false);
        };
        let Some(confirmed) = code.confirmed_guard_files.clone() else {
            return Task::ready(false);
        };
        let cached_layout = code.guard_layout.clone();
        let layout_version = code.guard_layout_version;
        let snapshot = worktree.read(cx).snapshot();
        let root = snapshot.abs_path().to_path_buf();
        cx.spawn(async move |this, cx| {
            let layout = match cached_layout {
                Some(layout) => layout,
                None => {
                    let layout = Arc::new(
                        cx.background_spawn(guard_layout(fs.clone(), snapshot))
                            .await,
                    );
                    this.update(cx, |this, _| {
                        if let Some(code) = this.worktrees.get_mut(&worktree_id)
                            && code.guard_layout_version == layout_version
                        {
                            code.guard_layout = Some(layout.clone());
                        }
                    })
                    .log_err();
                    layout
                }
            };
            let current = cx
                .background_spawn(async move {
                    hash_guard_files(fs.as_ref(), &root, &layout, names).await
                })
                .await;
            let confirmed = confirmed
                .into_iter()
                .filter(|(path, _)| {
                    is_guard_file(path, names) || is_guard_file(path, &[PROJECT_SETTINGS_FILE])
                })
                .collect::<GuardFiles>();
            current == confirmed
        })
    }

    /// Takes a change set of the worktree before the language servers hear of
    /// it, so that a writer's change pauses the worktree before it can reach
    /// a server running as allowed. The same set arriving again from the
    /// worktree store is ignored.
    pub(crate) fn observe_worktree_changes(
        &mut self,
        worktree_id: WorktreeId,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if code
            .last_observed_changes
            .as_ref()
            .is_some_and(|observed| Arc::ptr_eq(observed, changes))
        {
            return;
        }
        code.last_observed_changes = Some(changes.clone());
        self.on_entries_changed(worktree_id, changes, cx);
    }

    /// On the remote server: the IDE decides about the worktree at `root`
    /// before anything of it runs.
    pub fn expect_ide_decision(&mut self, root: PathBuf) {
        self.awaiting_ide.push(root);
    }

    fn track(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        let (worktree_id, abs_path) = {
            let worktree = worktree.read(cx);
            (worktree.id(), worktree.abs_path())
        };
        if self.worktrees.contains_key(&worktree_id) {
            return;
        }
        let mut code = WorktreeCode::new(abs_path.clone());
        match &self.role {
            Role::Local { fs } => {
                let fs = fs.clone();
                let root = abs_path;
                code._resolve_root = cx.spawn(async move |this, cx| {
                    let canonical = match fs.canonicalize(&root).await {
                        Ok(canonical) => canonical,
                        Err(error) => {
                            log::debug!("keeping {root:?} as opened: {error:#}");
                            root.to_path_buf()
                        }
                    };
                    this.update(cx, |this, cx| {
                        if let Some(code) = this.worktrees.get_mut(&worktree_id) {
                            code.stored_key = Some(stored_key(THIS_MACHINE, &canonical, true));
                            code.canonical_root = Some(canonical);
                        }
                        this.become_known(worktree_id, cx);
                    })
                    .log_err();
                });
            }
            // The remote server canonicalized the root already.
            Role::RemoteHost { host_identity, .. } => {
                code.stored_key = Some(stored_key(host_identity, &abs_path, false));
            }
            Role::Headless { .. } => {
                match self
                    .awaiting_ide
                    .iter()
                    .position(|root| same_path(root, &abs_path))
                {
                    Some(index) => {
                        self.awaiting_ide.remove(index);
                    }
                    None => code.known = true,
                }
                if let Some((generation, confirmed_generation)) =
                    self.announced.remove(&worktree_id)
                {
                    code.known = true;
                    code.generation = generation;
                    code.confirmed_generation = confirmed_generation;
                }
            }
            Role::Inert => code.known = true,
        }
        self.worktrees.insert(worktree_id, code);
        self.settle(worktree_id, WorktreeCodeState::Unknown, true, cx);
        self.become_known(worktree_id, cx);
    }

    /// Decides whether a worktree of this IDE starts paused, once what was
    /// kept, and under which key, is known.
    fn become_known(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        if !self.role.decides() {
            return;
        }
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if code.known {
            return;
        }
        match &self.stored_pauses {
            StoredPauses::Loading => return,
            StoredPauses::Unavailable => {}
            StoredPauses::Lost => {
                code.generation = code.generation.max(code.confirmed_generation + 1);
            }
            StoredPauses::Loaded(stored) => {
                let Some(key) = code.stored_key.as_ref() else {
                    return;
                };
                if let Some(stored_pause) = stored.get(key) {
                    code.generation = code.generation.max(code.confirmed_generation + 1);
                    code.confirmed_guard_files = stored_pause.confirmed_guard_files.clone();
                }
            }
        }
        code.known = true;
        self.settle(worktree_id, WorktreeCodeState::Unknown, true, cx);
    }

    fn finish_loading(&mut self, rows: Result<Vec<(String, String)>>, cx: &mut Context<Self>) {
        self.stored_pauses = match rows {
            Ok(rows) => StoredPauses::Loaded(
                rows.into_iter()
                    .filter_map(|(key, value)| {
                        let stored_pause = serde_json::from_str::<StoredPause>(&value)
                            .with_context(|| format!("reading the stored pause of {key:?}"))
                            .log_err()?;
                        Some((key, stored_pause))
                    })
                    .collect(),
            ),
            Err(error) => {
                log::error!("could not read which worktrees were left paused: {error:#}");
                StoredPauses::Lost
            }
        };
        let worktree_ids = self.worktrees.keys().copied().collect::<Vec<_>>();
        for worktree_id in worktree_ids {
            self.become_known(worktree_id, cx);
        }
    }

    fn advance(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let previous = code.state();
        code.generation += 1;
        self.settle(worktree_id, previous, true, cx);
        // An open resume dialog has to learn that there is more to review.
        if previous == WorktreeCodeState::Paused {
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
    }

    /// Carries out what a change of a worktree's state from `previous` means.
    /// `hash_guard_files` is false when a resume hashed them already.
    fn settle(
        &mut self,
        worktree_id: WorktreeId,
        previous: WorktreeCodeState,
        hash_guard_files: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        code.publish();
        let current = code.state();
        if current == previous {
            return;
        }
        match current {
            WorktreeCodeState::Unknown => {}
            WorktreeCodeState::Paused => {
                self.store_pause(worktree_id, cx);
                self.announce(worktree_id, cx);
                cx.emit(WorktreeCodeExecutionEvent::Paused(worktree_id));
            }
            WorktreeCodeState::Allowed => {
                code.paused_language_servers.clear();
                code.skipped_formatters.clear();
                if previous == WorktreeCodeState::Paused {
                    self.forget_stored_pause(worktree_id, cx);
                }
                self.announce(worktree_id, cx);
                if hash_guard_files {
                    self.hash_confirmed_guard_files(worktree_id, cx);
                }
                if previous == WorktreeCodeState::Paused {
                    cx.emit(WorktreeCodeExecutionEvent::Resumed(worktree_id));
                }
            }
        }
    }

    fn store_pause(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(code) = self.worktrees.get(&worktree_id) else {
            return;
        };
        let Some(key) = code.stored_key.clone() else {
            return;
        };
        let stored_pause = StoredPause {
            confirmed_guard_files: code.confirmed_guard_files.clone(),
        };
        let StoredPauses::Loaded(stored) = &mut self.stored_pauses else {
            return;
        };
        stored.insert(key.clone(), stored_pause.clone());
        let value = match serde_json::to_string(&stored_pause) {
            Ok(value) => value,
            Err(error) => {
                log::error!("could not record that {key:?} is paused: {error}");
                return;
            }
        };
        let store = match key_value_store(cx) {
            Ok(store) => store,
            Err(error) => {
                log::error!("could not record that {key:?} is paused: {error:#}");
                return;
            }
        };
        cx.background_spawn(async move {
            store
                .scoped(DB_NAMESPACE)
                .write(key, value)
                .await
                .context("recording that a worktree's code is paused")
        })
        .detach_and_log_err(cx);
    }

    fn forget_stored_pause(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(key) = self
            .worktrees
            .get(&worktree_id)
            .and_then(|code| code.stored_key.clone())
        else {
            return;
        };
        let StoredPauses::Loaded(stored) = &mut self.stored_pauses else {
            return;
        };
        stored.remove(&key);
        let store = match key_value_store(cx) {
            Ok(store) => store,
            Err(error) => {
                log::error!("could not forget that {key:?} was paused: {error:#}");
                return;
            }
        };
        cx.background_spawn(async move {
            store
                .scoped(DB_NAMESPACE)
                .delete(key)
                .await
                .context("forgetting that a worktree's code was paused")
        })
        .detach_and_log_err(cx);
    }

    /// Tells the remote host what its worktree's code may do, and learns from
    /// its acknowledgement whether it follows.
    fn announce(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Role::RemoteHost {
            client,
            project_id,
            connected,
            ..
        } = &self.role
        else {
            return;
        };
        // A reconnection announces every worktree again.
        if !connected {
            return;
        }
        let Some(code) = self.worktrees.get(&worktree_id) else {
            return;
        };
        if !code.known {
            return;
        }
        let request = client.request(proto::SetWorktreeCodeExecution {
            project_id: *project_id,
            worktree_id: worktree_id.to_proto(),
            generation: code.generation,
            confirmed_generation: code.confirmed_generation,
            language_servers_allowed_while_paused: self.allowed_language_servers.clone(),
        });
        cx.spawn(async move |this, cx| {
            let timeout = cx
                .background_executor()
                .timer(REMOTE_ACKNOWLEDGEMENT_TIMEOUT);
            let host_follows = futures::select_biased! {
                response = request.fuse() => match response {
                    Ok(_) => RemoteHostPause::Following,
                    Err(error) => {
                        log::warn!(
                            "the remote server does not follow the worktree pause: {error:#}"
                        );
                        RemoteHostPause::NotFollowing
                    }
                },
                _ = timeout.fuse() => {
                    log::warn!(
                        "the remote server did not acknowledge the worktree pause, so it is from before ADR-F50"
                    );
                    RemoteHostPause::NotFollowing
                }
            };
            this.update(cx, |this, cx| this.host_answered(host_follows, cx))
                .log_err();
        })
        .detach();
    }

    fn host_answered(&mut self, answer: RemoteHostPause, cx: &mut Context<Self>) {
        let Role::RemoteHost { host_follows, .. } = &mut self.role else {
            return;
        };
        // One acknowledgement shows that the server follows; a later timeout
        // is a slow connection rather than an old server.
        if *host_follows == RemoteHostPause::Following || *host_follows == answer {
            return;
        }
        *host_follows = answer;
        let worktree_ids = self.worktrees.keys().copied().collect::<Vec<_>>();
        for worktree_id in worktree_ids {
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
    }

    fn on_connection_changed(
        &mut self,
        remote_client: Entity<RemoteClient>,
        cx: &mut Context<Self>,
    ) {
        let now_connected = remote_client.read(cx).connection_state() == ConnectionState::Connected;
        let Role::RemoteHost {
            connected,
            host_follows,
            ..
        } = &mut self.role
        else {
            return;
        };
        let reconnected = now_connected && !*connected;
        *connected = now_connected;
        if reconnected {
            // The server may be another one now.
            *host_follows = RemoteHostPause::Unknown;
            let worktree_ids = self.worktrees.keys().copied().collect::<Vec<_>>();
            for worktree_id in worktree_ids {
                self.announce(worktree_id, cx);
            }
        }
    }

    fn on_settings_changed(&mut self, cx: &mut Context<Self>) {
        let allowed_language_servers = language_servers_allowed_in_user_settings(cx);
        if allowed_language_servers != self.allowed_language_servers {
            self.allowed_language_servers = allowed_language_servers;
            let worktree_ids = self.worktrees.keys().copied().collect::<Vec<_>>();
            for worktree_id in worktree_ids {
                self.announce(worktree_id, cx);
            }
            cx.emit(WorktreeCodeExecutionEvent::LanguageServersAllowedWhilePausedChanged);
        }
        let enabled = pause_after_agent_writes_enabled(cx);
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            // Turned off in the user's own settings: what the pause holds is
            // released as if the user had resumed it.
            for worktree_id in self.paused_worktrees() {
                self.release(worktree_id, cx);
            }
        }
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => self.track(worktree, cx),
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id)
            | WorktreeStoreEvent::WorktreeReleased(_, worktree_id) => {
                self.worktrees.remove(worktree_id);
            }
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, changes) => {
                self.observe_worktree_changes(*worktree_id, changes, cx);
            }
            WorktreeStoreEvent::WorktreeOrderChanged
            | WorktreeStoreEvent::WorktreeUpdateSent(_)
            | WorktreeStoreEvent::WorktreeUpdatedGitRepositories(..)
            | WorktreeStoreEvent::WorktreeDeletedEntry(..)
            | WorktreeStoreEvent::WorktreeUpdatedRootRepoCommonDir(_) => {}
        }
    }

    fn on_entries_changed(
        &mut self,
        worktree_id: WorktreeId,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        if changes.is_empty() {
            return;
        }
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let guard_file_changed = changes
            .iter()
            .any(|(path, _, _)| is_guard_file_trigger(path.as_unix_str()));
        // Where guard files are looked for moves with manifests and with the
        // root's directories.
        if guard_file_changed
            || changes
                .iter()
                .any(|(path, _, _)| !path.as_unix_str().contains('/'))
        {
            code.guard_layout = None;
            code.guard_layout_version += 1;
        }
        // The watcher cannot tell who wrote a file, so while a writer runs,
        // every change is taken for one of its writes.
        if self.enabled && code.writer_running() {
            self.advance(worktree_id, cx);
        }
        // The user's own change: what they confirmed moves with it. Not while
        // an external agent's session lasts, whose processes can write
        // between its turns, and not on the remote server, which takes its
        // hashes only when the IDE confirms (a sandboxed command can write
        // there before the IDE's pause arrives). While the worktree is
        // paused, the language server store checks each change set itself.
        if guard_file_changed
            && self.state(worktree_id) == WorktreeCodeState::Allowed
            && matches!(self.role, Role::Local { .. })
        {
            if self.external_agent_session_alive() {
                log::info!(
                    "an external agent's session lasts, so changed guard files of {worktree_id:?} are not taken as confirmed"
                );
            } else {
                self.hash_confirmed_guard_files(worktree_id, cx);
            }
        }
    }

    fn forget_ended_writers(&mut self, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        for (worktree_id, code) in &mut self.worktrees {
            let before = code.writers.len();
            code.writers.retain(|writer| writer.strong_count() > 0);
            if code.writers.len() != before {
                changed.push(*worktree_id);
            }
        }
        for worktree_id in changed {
            // What the writer changed where the scan does not look is only
            // found on disk.
            if self.is_paused(worktree_id) {
                cx.emit(WorktreeCodeExecutionEvent::CheckGuardFiles(worktree_id));
            }
            cx.emit(WorktreeCodeExecutionEvent::DetailsChanged(worktree_id));
        }
    }

    fn hash_guard_files_now(&self, worktree_id: WorktreeId, cx: &App) -> Option<Task<GuardFiles>> {
        let fs = self.role.fs().cloned()?;
        let worktree = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)?;
        let snapshot = worktree.read(cx).snapshot();
        Some(cx.background_spawn(current_guard_files(fs, snapshot, ON_DISK_GUARD_FILES)))
    }

    /// Takes the hashes a restricted language server is checked against, once
    /// the worktree's first scan has found its files. Discarded if the
    /// worktree is paused before they are taken.
    fn hash_confirmed_guard_files(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let Some(fs) = self.role.fs().cloned() else {
            return;
        };
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return;
        };
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let generation = code.generation;
        code.confirmed_guard_files = None;
        let scan_complete = worktree
            .read(cx)
            .as_local()
            .map(|local_worktree| local_worktree.scan_complete());
        code._hash_guard_files = cx.spawn(async move |this, cx| {
            if let Some(scan_complete) = scan_complete {
                scan_complete.await;
            }
            let snapshot = worktree.read_with(cx, |worktree, _| worktree.snapshot());
            let hashes = cx
                .background_spawn(current_guard_files(fs, snapshot, ON_DISK_GUARD_FILES))
                .await;
            this.update(cx, |this, _| {
                if let Some(code) = this.worktrees.get_mut(&worktree_id)
                    && code.generation == generation
                    && code.state() == WorktreeCodeState::Allowed
                {
                    code.confirmed_guard_files = Some(hashes);
                }
            })
            .log_err();
        });
    }

    async fn handle_set_worktree_code_execution(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetWorktreeCodeExecution>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        #[cfg(any(test, feature = "test-support"))]
        if cx.update(|cx| cx.has_global::<IgnoreWorktreeCodeExecutionForTests>()) {
            drop(this);
            return futures::future::pending().await;
        }
        let payload = envelope.payload;
        this.update(&mut cx, |this, cx| {
            if this.allowed_language_servers != payload.language_servers_allowed_while_paused {
                this.allowed_language_servers = payload.language_servers_allowed_while_paused;
                cx.emit(WorktreeCodeExecutionEvent::LanguageServersAllowedWhilePausedChanged);
            }
            this.follow_ide(
                WorktreeId::from_proto(payload.worktree_id),
                payload.generation,
                payload.confirmed_generation,
                cx,
            )
        });
        Ok(proto::Ack {})
    }

    fn follow_ide(
        &mut self,
        worktree_id: WorktreeId,
        generation: u64,
        confirmed_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(code) = self.worktrees.get_mut(&worktree_id) else {
            self.announced
                .insert(worktree_id, (generation, confirmed_generation));
            return;
        };
        let previous = code.state();
        let written_again = previous == WorktreeCodeState::Paused && generation != code.generation;
        code.known = true;
        code.generation = generation;
        code.confirmed_generation = confirmed_generation;
        self.settle(worktree_id, previous, true, cx);
        if written_again {
            cx.emit(WorktreeCodeExecutionEvent::CheckGuardFiles(worktree_id));
        }
    }
}

fn refused_resume(code: &WorktreeCode, seen_generation: u64) -> Option<ResumeOutcome> {
    if code.state() != WorktreeCodeState::Paused {
        Some(ResumeOutcome::NotPaused)
    } else if code.generation != seen_generation {
        Some(ResumeOutcome::WrittenSinceShown)
    } else if code.writer_running() {
        Some(ResumeOutcome::WriterRunning)
    } else {
        None
    }
}

/// ADR-F50: whether an external formatter may run while its worktree's code
/// is paused: one of [`FORMATTERS_ALLOWED_WHILE_PAUSED`] (`clang-format-18`
/// and the like included), run from an executable outside the worktree at
/// `worktree_root`, where it starts. `search_path` is the `PATH` a bare name
/// is looked up on.
pub fn external_formatter_may_run_while_paused(
    command: &str,
    search_path: Option<&str>,
    worktree_root: &Path,
) -> bool {
    let stem = program_stem(command);
    let allowed = FORMATTERS_ALLOWED_WHILE_PAUSED.iter().any(|name| {
        stem == *name
            || stem
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|version| {
                    !version.is_empty()
                        && version
                            .chars()
                            .all(|character| character.is_ascii_digit() || character == '.')
                })
    });
    if !allowed {
        return false;
    }
    let program = Path::new(command);
    if program.has_root() {
        return !path_is_within(program, worktree_root);
    }
    // Relative to the worktree, where the formatter starts.
    if program.components().count() > 1 {
        return false;
    }
    if util::command::resolve_command_next_to_current_exe(command).is_some()
        || dev_tools::resolve_managed_tool(command).is_some()
    {
        return true;
    }
    // A bare name is looked up on `PATH`, which the worktree's environment
    // can point into. An empty or relative entry is searched from where the
    // program starts, which is the worktree.
    let Some(search_path) = search_path else {
        return false;
    };
    if std::env::split_paths(search_path).any(|entry| !entry.has_root()) {
        return false;
    }
    match which::which_in(command, Some(search_path), worktree_root) {
        Ok(found) => !path_is_within(&found, worktree_root),
        Err(_) => false,
    }
}

/// The paths among `paths` that are spelled otherwise once links, junctions
/// and mapped drives on the way to them are resolved, as resolved: the
/// nearest part of each that exists is resolved, and the rest kept.
pub(crate) async fn canonical_write_paths(fs: &dyn Fs, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical = Vec::new();
    for path in paths {
        if let Some(resolved) = canonicalize_nearest_existing(fs, path).await
            && !same_path(&resolved, path)
        {
            canonical.push(resolved);
        }
    }
    canonical
}

async fn canonicalize_nearest_existing(fs: &dyn Fs, path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut rest = Vec::<OsString>::new();
    loop {
        if let Ok(canonical) = fs.canonicalize(existing).await {
            return Some(
                rest.iter()
                    .rev()
                    .fold(canonical, |resolved, component| resolved.join(component)),
            );
        }
        rest.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
}

/// Whether a change to the worktree-relative `path` (Unix separators) can
/// change the guard files or where they are looked for.
fn is_guard_file_trigger(path: &str) -> bool {
    is_guard_file(path, GUARD_FILE_TRIGGERS)
}

/// Whether `path` is `root` or inside it (see [`comparable_path`]).
pub(crate) fn path_is_within(path: &Path, root: &Path) -> bool {
    comparable_path(path).starts_with(comparable_path(root))
}

/// `path` as compared: `..` taken away, Windows' `\\?\` ignored, `\\wsl$\`
/// taken for `\\wsl.localhost\`, and without regard to case where the
/// filesystem usually has none.
fn comparable_path(path: &Path) -> PathBuf {
    let normalized = lexically_normalized(SanitizedPath::new(path).as_path());
    if !cfg!(any(target_os = "windows", target_os = "macos")) {
        return normalized;
    }
    let text = normalized.to_string_lossy().to_lowercase();
    match text.strip_prefix(r"\\wsl$\") {
        Some(rest) if cfg!(target_os = "windows") => {
            PathBuf::from(format!(r"\\wsl.localhost\{rest}"))
        }
        _ => PathBuf::from(text),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_is_within(left, right) && path_is_within(right, left)
}

fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component),
        }
    }
    normalized
}

/// The key a worktree's pause is kept under: its host and its root, the
/// same however the root was spelled when it was opened.
fn stored_key(host: &str, root: &Path, on_this_machine: bool) -> String {
    let root = lexically_normalized(SanitizedPath::new(root).as_path());
    let root = root.to_string_lossy();
    let root = if on_this_machine {
        fold_case(&root)
    } else {
        Cow::Borrowed(root.as_ref())
    };
    format!("{host}\n{root}")
}

/// `text` lowercased where the filesystem usually ignores case.
fn fold_case(text: &str) -> Cow<'_, str> {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        Cow::Owned(text.to_lowercase())
    } else {
        Cow::Borrowed(text)
    }
}

fn names_match(actual: &str, wanted: &str) -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        actual.eq_ignore_ascii_case(wanted)
    } else {
        actual == wanted
    }
}

/// Whether the worktree-relative `path` (Unix separators) ends with one of
/// `names` at a component boundary, without regard to case where the
/// filesystem usually has none.
fn is_guard_file(path: &str, names: &[&str]) -> bool {
    let path = fold_case(path);
    names.iter().any(|name| {
        let name = fold_case(name);
        *path == *name
            || path
                .strip_suffix(name.as_ref())
                .is_some_and(|parent| parent.ends_with('/'))
    })
}

/// Where the worktree's guard files are looked for. The scan's entries are
/// looked through, and, since the scan leaves out ignored directories and
/// excluded files and does not follow links out of the worktree, the paths
/// rust-analyzer and Cargo read them from are looked up on disk too: in the
/// root and each directory below it (rust-analyzer finds workspaces there),
/// and in each directory with a `Cargo.toml` and those above it (Cargo and
/// rustup look upwards from where they start).
async fn guard_layout(fs: Arc<dyn Fs>, snapshot: Snapshot) -> GuardLayout {
    let mut layout = GuardLayout::default();
    // A single file's worktree holds no guard files.
    if !snapshot.root_entry().is_some_and(|entry| entry.is_dir()) {
        return layout;
    }
    layout.directories.insert(String::new());
    if let Ok(mut children) = fs.read_dir(snapshot.abs_path()).await {
        while let Some(child) = children.next().await {
            let Ok(child) = child else {
                continue;
            };
            if fs.is_dir(&child).await
                && let Some(name) = child.file_name()
            {
                layout
                    .directories
                    .insert(name.to_string_lossy().into_owned());
            }
        }
    }
    for entry in snapshot.entries(true, 0) {
        if !entry.is_file() {
            continue;
        }
        let path = entry.path.as_unix_str();
        if is_guard_file(path, ON_DISK_GUARD_FILES) || is_guard_file(path, &[PROJECT_SETTINGS_FILE])
        {
            layout.scanned.push(path.to_string());
        }
        let (parent, file_name) = path.rsplit_once('/').unwrap_or(("", path));
        if names_match(file_name, "Cargo.toml") {
            let mut directory = String::new();
            for component in parent.split('/').filter(|component| !component.is_empty()) {
                if !directory.is_empty() {
                    directory.push('/');
                }
                directory.push_str(component);
                layout.directories.insert(directory.clone());
            }
        }
    }
    layout
}

/// The guard files named in `names` (and the project settings files) as they
/// are on disk now, looked for as `layout` says.
async fn hash_guard_files(
    fs: &dyn Fs,
    root: &Path,
    layout: &GuardLayout,
    names: &[&str],
) -> GuardFiles {
    let absolute = |relative: &str| {
        relative
            .split('/')
            .fold(root.to_path_buf(), |path, component| path.join(component))
    };
    let mut hashes = GuardFiles::new();
    for path in &layout.scanned {
        if is_guard_file(path, names) || is_guard_file(path, &[PROJECT_SETTINGS_FILE]) {
            let hash = hash_file(fs, &absolute(path)).await;
            hashes.insert(fold_case(path).into_owned(), hash);
        }
    }
    for directory in &layout.directories {
        for name in names {
            let relative = if directory.is_empty() {
                (*name).to_string()
            } else {
                format!("{directory}/{name}")
            };
            let path = absolute(&relative);
            if let btree_map::Entry::Vacant(vacant) =
                hashes.entry(fold_case(&relative).into_owned())
                && fs.is_file(&path).await
            {
                vacant.insert(hash_file(fs, &path).await);
            }
        }
    }
    hashes
}

async fn current_guard_files(
    fs: Arc<dyn Fs>,
    snapshot: Snapshot,
    names: &'static [&'static str],
) -> GuardFiles {
    let root = snapshot.abs_path().to_path_buf();
    let layout = guard_layout(fs.clone(), snapshot).await;
    hash_guard_files(fs.as_ref(), &root, &layout, names).await
}

async fn hash_file(fs: &dyn Fs, path: &Path) -> Option<String> {
    let bytes = fs.load_bytes(path).await.ok()?;
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

fn key_value_store(cx: &App) -> Result<KeyValueStore> {
    cx.try_global::<db::AppDatabase>()
        .map(KeyValueStore::from_app_db)
        .context("the database is not open")
}
