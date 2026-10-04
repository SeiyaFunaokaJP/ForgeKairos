//! ADR-F68: the name-based symbol index of a project's C and C++ files.
//!
//! The machine that holds the files (this one, or the remote server) keeps one
//! [`SymbolIndex`] per worktree and answers from it; a client of a remote
//! server asks over the wire, and a collaboration guest has none. Results are
//! name matches, not what a language server resolved, so callers show them as
//! such and prefer the language server's answer when there is one.

use std::{
    collections::BTreeSet,
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, anyhow};
use collections::HashMap;
use fs::Fs;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription,
    Task, TaskExt as _, WeakEntity,
};
use language::{Buffer, BufferEvent};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use settings::{Settings as _, SettingsLocation, SettingsStore, SymbolIndexEnabled};
use sha2::{Digest as _, Sha256};
use symbol_index::{
    Definition, FileEntry, FileSource, IndexConfig, Position, Progress, ProgressPhase,
    StaleIndexOutcome, SymbolIndex, is_indexable_path,
};
use text::{BufferId, Point};
use util::{ResultExt as _, paths::PathMatcher, rel_path::RelPath};
use worktree::{PathChange, Snapshot, UpdatedEntriesSet, Worktree, WorktreeId, WorktreeSettings};

use crate::{
    ProjectPath,
    buffer_store::{BufferStore, BufferStoreEvent},
    project_settings::ProjectSettings,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

pub use symbol_index::{
    AssignmentKind, CallKind, CalleeTarget, DefinitionKind, SearchMode, SourceLanguage,
};

mod symbol_index_types;
pub use symbol_index_types::*;

/// File system events are gathered for this long before the index reads them.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(500);
/// More changed files than this (a branch switch) are applied by comparing the
/// whole worktree, which reads only what really changed.
const FULL_UPDATE_THRESHOLD: usize = 2000;
/// The most call sites one `callers` answer carries, over the wire or not.
pub const MAX_CALLERS_LIMIT: usize = 5000;
/// The most matches one `search` answer carries.
pub const MAX_SEARCH_LIMIT: usize = 1000;
/// The most candidates one `definitions` answer carries; a name defined more
/// often than this is better searched for.
pub const MAX_DEFINITIONS: usize = 1000;
/// The most stores one `assignments` answer carries (a member such as `open`
/// can be assigned in thousands of places).
pub const MAX_ASSIGNMENTS: usize = 5000;
/// A remote server from before these requests never answers them.
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const IN_USE_RETRIES: usize = 10;
/// The index of a folder not opened for this long is removed.
pub const UNUSED_INDEX_AGE: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// Unused indexes are looked for once per process, by the first store that has an index
/// directory.
static UNUSED_INDEXES_LOOKED_FOR: AtomicBool = AtomicBool::new(false);
/// How long after the first store is made unused indexes are looked for.
const UNUSED_INDEX_REMOVAL_DELAY: Duration = Duration::from_secs(60);
const IN_USE_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolIndexStatus {
    /// Turned off, nothing to index (no C or C++ files under `"auto"`), or a
    /// worktree the index does not cover (a single file).
    Disabled,
    /// Waiting for the worktree's first scan.
    Scanning,
    Indexing {
        done: usize,
        total: usize,
    },
    Ready {
        stats: SymbolIndexStats,
    },
    Unavailable {
        reason: SymbolIndexUnavailable,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymbolIndexStats {
    pub files: u64,
    pub definitions: u64,
    pub calls: u64,
    pub assignments: u64,
    pub files_with_syntax_errors: u64,
    pub disk_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolIndexUnavailable {
    /// Another Zed window (or process) has the index of this folder open.
    InUseByAnotherWindow,
    /// The remote server predates the symbol index.
    UnsupportedHost,
    /// A project shared by someone else.
    Collaboration,
    Error(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SymbolIndexStoreEvent {
    StatusChanged(WorktreeId),
}

/// A definition or declaration the index found, located in the project.
/// Positions are 0-based rows and UTF-8 byte columns, as [`Point`] counts
/// them, taken from the file on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedDefinition {
    pub name: String,
    pub kind: DefinitionKind,
    pub is_static: bool,
    pub is_declaration: bool,
    pub language: SourceLanguage,
    pub path: ProjectPath,
    /// The declared name.
    pub name_range: Range<Point>,
    /// The whole definition, including a function's body.
    pub item_range: Range<Point>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedCallSite {
    pub path: ProjectPath,
    /// The range of the called name.
    pub range: Range<Point>,
    pub callee: String,
    pub kind: CallKind,
    pub in_macro_body: bool,
    /// The function (or macro) containing the call; `None` at file scope.
    pub caller: Option<IndexedDefinition>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexedCallers {
    pub calls: Vec<IndexedCallSite>,
    /// More call sites exist than the limit allowed.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedCalleeSite {
    pub path: ProjectPath,
    pub range: Range<Point>,
    pub callee: String,
    pub kind: CallKind,
    pub in_macro_body: bool,
    pub target: CalleeTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedCallees {
    /// The function containing the position that was asked about.
    pub function: IndexedDefinition,
    pub callees: Vec<IndexedCalleeSite>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedAssignment {
    pub path: ProjectPath,
    /// The range of the assigned function name.
    pub range: Range<Point>,
    pub field: String,
    pub function: String,
    pub container: Option<String>,
    pub kind: AssignmentKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSymbolMatch {
    pub definition: IndexedDefinition,
    pub score: u32,
}

pub struct SymbolIndexStore {
    worktree_store: Entity<WorktreeStore>,
    state: StoreState,
    /// The index a hover made of files the worktree's index leaves out.
    scratch: Option<Arc<symbol_index_types::ScratchIndex>>,
    _subscriptions: Vec<Subscription>,
}

enum StoreState {
    Local(LocalStore),
    Remote(RemoteStore),
    Unavailable(SymbolIndexUnavailable),
}

struct LocalStore {
    fs: Arc<dyn Fs>,
    /// Type queries read the project's edited (not saved) C and C++ buffers instead of the
    /// files on disk.
    buffer_store: WeakEntity<BufferStore>,
    /// When each open buffer was last edited, in edit order: a type query that may read only
    /// some of the edited buffers reads the most recently edited ones.
    buffer_edits: HashMap<BufferId, u64>,
    buffer_edit_count: u64,
    buffer_subscriptions: HashMap<BufferId, Subscription>,
    /// Where each worktree's index directory goes. `None` indexes nothing,
    /// which is what a project on a fake file system, and any project in a
    /// test, gets unless the test says otherwise, so that tests never write
    /// into the user's data directory.
    index_root: Option<PathBuf>,
    worktrees: HashMap<WorktreeId, WorktreeIndex>,
    downstream: Option<(AnyProtoClient, u64)>,
}

struct RemoteStore {
    client: AnyProtoClient,
    project_id: u64,
    support: RemoteSupport,
    statuses: HashMap<WorktreeId, SymbolIndexStatus>,
    _probe: Option<Task<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteSupport {
    Unknown,
    Supported,
    Unsupported,
}

/// What the index of one worktree was last built with; a change rereads it.
#[derive(Clone, PartialEq)]
struct AppliedSettings {
    enabled: SymbolIndexEnabled,
    include_ignored: bool,
    rules: Arc<PathRules>,
}

struct WorktreeIndex {
    worktree: WeakEntity<Worktree>,
    root: Arc<Path>,
    status: SymbolIndexStatus,
    index: Option<Arc<SymbolIndex>>,
    applied: Option<AppliedSettings>,
    /// Waiting for the worktree's first scan; changes before it are covered
    /// by the full update that follows it.
    initial_scan_done: bool,
    job_running: bool,
    pending: PendingWork,
    cancel: Arc<AtomicBool>,
    _scan: Option<Task<()>>,
    _debounce: Option<Task<()>>,
    _job: Option<Task<()>>,
    _save: Option<Task<()>>,
}

impl Drop for WorktreeIndex {
    fn drop(&mut self) {
        // The update thread holds the index; it stops at the next file and
        // releases the directory's lock when it ends.
        self.cancel.store(true, Ordering::SeqCst);
        close_index_in_background(self.index.take());
    }
}

/// Closing an index saves what it has not saved yet (a large file, flushed to disk), which the
/// foreground thread must not wait for. Opening the index again meanwhile waits for its lock.
fn close_index_in_background(index: Option<Arc<SymbolIndex>>) {
    let Some(index) = index else {
        return;
    };
    if let Err(error) = std::thread::Builder::new()
        .name("symbol-index-close".to_string())
        .spawn(move || drop(index))
    {
        log::error!("symbol_index: cannot start a thread to close an index: {error}");
    }
}

#[derive(Default)]
struct PendingWork {
    full: bool,
    changed: BTreeSet<Arc<RelPath>>,
    removed: BTreeSet<Arc<RelPath>>,
}

impl PendingWork {
    fn is_empty(&self) -> bool {
        !self.full && self.changed.is_empty() && self.removed.is_empty()
    }
}

enum JobKind {
    Full,
    Files {
        changed: Vec<Arc<RelPath>>,
        removed: Vec<Arc<RelPath>>,
    },
}

struct Job {
    kind: JobKind,
    snapshot: Snapshot,
    include_ignored: bool,
    auto: bool,
    index: Option<Arc<SymbolIndex>>,
    index_root: PathBuf,
    source: WorktreeFileSource,
    cancel: Arc<AtomicBool>,
}

enum JobMessage {
    Opened(Arc<SymbolIndex>),
    Progress { done: usize, total: usize },
    Finished(JobOutcome),
}

enum JobOutcome {
    Updated(SymbolIndexStats),
    NothingToIndex,
    Cancelled,
    OpenFailed(SymbolIndexUnavailable),
    UpdateFailed(String),
}

/// Reads worktree files for the index, which reads from its own threads and
/// cannot await. A real file system is read directly rather than through
/// [`Fs`], whose reads hop onto the application's background threads; a fake
/// one only exists behind [`Fs`].
struct WorktreeFileSource {
    fs: Arc<dyn Fs>,
    root: Arc<Path>,
    /// The root with links resolved, known once the update thread has asked
    /// the file system. Every file read must resolve to a path under it.
    canonical_root: Option<PathBuf>,
    rules: Arc<PathRules>,
    read_directly: bool,
}

impl WorktreeFileSource {
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.read_directly {
            std::fs::canonicalize(path)
        } else {
            futures::executor::block_on(self.fs.canonicalize(path)).map_err(|error| {
                let missing = futures::executor::block_on(self.fs.metadata(path))
                    .is_ok_and(|metadata| metadata.is_none());
                if missing {
                    io::Error::new(io::ErrorKind::NotFound, format!("{error:#}"))
                } else {
                    io::Error::other(format!("{error:#}"))
                }
            })
        }
    }

    /// Where `relative` really is, if the index may read it. The worktree's
    /// entry was checked when the file was listed, but the file may have been
    /// replaced by a link since, and a link inside the worktree may lead to a
    /// file that is private or excluded under its own name. Such files count
    /// as gone, so that the index drops what it held of them.
    fn resolve(&self, relative: &RelPath) -> io::Result<PathBuf> {
        let refused = |why: &str| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} {why}", relative.as_unix_str()),
            )
        };
        let canonical_root = self
            .canonical_root
            .as_ref()
            .ok_or_else(|| io::Error::other("the worktree root was not resolved"))?;
        let canonical = self.canonicalize(&self.root.join(relative.as_std_path()))?;
        let target = canonical
            .strip_prefix(canonical_root)
            .map_err(|_| refused("leads outside the worktree"))?;
        let target = RelPath::new(target, util::paths::PathStyle::local())
            .map_err(|_| refused("leads to a path the worktree cannot name"))?;
        if target.as_ref() != relative && self.rules.hides(&target) {
            return Err(refused("leads to an excluded or private file"));
        }
        Ok(canonical)
    }
}

impl FileSource for WorktreeFileSource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let relative = RelPath::from_unix_str(path)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{error:#}")))?;
        let absolute = self.resolve(relative)?;
        // One byte over the limit is enough for the index to skip the file,
        // without holding a huge (or endless) file in memory.
        let limit = IndexConfig::default().max_file_bytes.saturating_add(1);
        if self.read_directly {
            use std::io::Read as _;
            let mut contents = Vec::new();
            std::fs::File::open(&absolute)?
                .take(limit)
                .read_to_end(&mut contents)?;
            return Ok(contents);
        }
        match futures::executor::block_on(self.fs.load_bytes(&absolute)) {
            Ok(mut bytes) => {
                bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
                Ok(bytes)
            }
            Err(error) => {
                let missing = futures::executor::block_on(self.fs.metadata(&absolute))
                    .is_ok_and(|metadata| metadata.is_none());
                if missing {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{error:#}"),
                    ))
                } else {
                    Err(io::Error::other(format!("{error:#}")))
                }
            }
        }
    }
}

/// Which paths of a worktree `file_scan_exclusions` and `private_files` hide,
/// including what the `.zed/settings.json` files of its folders add. The
/// worktree marks its entries with the root's settings only.
#[derive(Clone, PartialEq)]
struct PathRules {
    global: (PathMatcher, PathMatcher),
    /// Folders with settings of their own, in the settings store's order.
    folders: Vec<(Arc<RelPath>, PathMatcher, PathMatcher)>,
}

impl PathRules {
    fn for_worktree(worktree_id: WorktreeId, cx: &App) -> Self {
        let global = WorktreeSettings::get_global(cx);
        Self {
            global: (
                global.file_scan_exclusions.clone(),
                global.private_files.clone(),
            ),
            folders: cx
                .global::<SettingsStore>()
                .get_all_locals::<WorktreeSettings>()
                .into_iter()
                .filter(|(id, _, _)| *id == worktree_id)
                .map(|(_, folder, settings)| {
                    (
                        folder,
                        settings.file_scan_exclusions.clone(),
                        settings.private_files.clone(),
                    )
                })
                .collect(),
        }
    }

    /// Mirrors how the settings store picks the settings of a path: those of
    /// the deepest folder containing it that has any.
    fn hides(&self, path: &RelPath) -> bool {
        let (exclusions, private_files) = self
            .folders
            .iter()
            .rev()
            .find(|(folder, _, _)| path.starts_with(folder))
            .map_or(
                (&self.global.0, &self.global.1),
                |(_, exclusions, private_files)| (exclusions, private_files),
            );
        path.ancestors()
            .any(|ancestor| exclusions.is_match(ancestor) || private_files.is_match(ancestor))
    }
}

impl EventEmitter<SymbolIndexStoreEvent> for SymbolIndexStore {}

impl SymbolIndexStore {
    /// Handlers for a client's questions, on the machine that holds the files.
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_get_status);
        client.add_entity_request_handler(Self::handle_definitions);
        client.add_entity_request_handler(Self::handle_callers);
        client.add_entity_request_handler(Self::handle_callees);
        client.add_entity_request_handler(Self::handle_assignments);
        client.add_entity_request_handler(Self::handle_search);
        client.add_entity_request_handler(Self::handle_rebuild);
        client.add_entity_request_handler(Self::handle_describe_type);
        client.add_entity_request_handler(Self::handle_resolve_type);
        client.add_entity_request_handler(Self::handle_expand_member);
        client.add_entity_request_handler(Self::handle_type_tree);
    }

    /// Handlers on a client of a remote server.
    pub fn init_remote(client: &AnyProtoClient) {
        client.add_entity_message_handler(Self::handle_update_status);
    }

    /// The index directory Zed uses on this machine. None in tests, also on the real file
    /// system, so that they never write into the user's data directory; a test that needs an
    /// index calls [`Self::set_index_root_for_tests`] with a directory of its own.
    pub fn default_index_root(fs: &dyn Fs) -> Option<PathBuf> {
        if cfg!(any(test, feature = "test-support")) {
            return None;
        }
        (!fs.is_fake()).then(|| paths::data_dir().join("symbol_index"))
    }

    pub fn local(
        worktree_store: Entity<WorktreeStore>,
        buffer_store: WeakEntity<BufferStore>,
        fs: Arc<dyn Fs>,
        index_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![
            cx.subscribe(&worktree_store, Self::on_worktree_store_event),
            cx.observe_global::<SettingsStore>(Self::on_settings_changed),
        ];
        let open_buffers = match buffer_store.upgrade() {
            Some(buffer_store) => {
                subscriptions.push(cx.subscribe(&buffer_store, Self::on_buffer_store_event));
                buffer_store.read(cx).buffers().collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        let mut this = Self {
            worktree_store: worktree_store.clone(),
            state: StoreState::Local(LocalStore {
                fs,
                buffer_store,
                buffer_edits: HashMap::default(),
                buffer_edit_count: 0,
                buffer_subscriptions: HashMap::default(),
                index_root,
                worktrees: HashMap::default(),
                downstream: None,
            }),
            scratch: None,
            _subscriptions: subscriptions,
        };
        for buffer in &open_buffers {
            this.watch_buffer_edits(buffer, cx);
        }
        let worktrees = worktree_store.read(cx).worktrees().collect::<Vec<_>>();
        for worktree in worktrees {
            this.add_local_worktree(&worktree, cx);
        }
        // Not in tests: a test project on the real file system has the user's own index
        // directory.
        if !cfg!(any(test, feature = "test-support"))
            && let StoreState::Local(local) = &this.state
            && local.index_root.is_some()
            && !UNUSED_INDEXES_LOOKED_FOR.swap(true, Ordering::SeqCst)
        {
            cx.spawn(async move |this, cx| -> Result<()> {
                // Out of the way of opening the windows and their indexes.
                cx.background_executor()
                    .timer(UNUSED_INDEX_REMOVAL_DELAY)
                    .await;
                let removal = this.update(cx, |this, cx| {
                    this.remove_unused_indexes(SystemTime::now(), cx)
                })?;
                removal.await;
                Ok(())
            })
            .detach_and_log_err(cx);
        }
        this
    }

    /// Removes the indexes under the index directory that were last opened
    /// [`UNUSED_INDEX_AGE`] before `now`, except those open here, those another process (or
    /// window) holds, and directories with anything an index does not write.
    pub fn remove_unused_indexes(
        &self,
        now: SystemTime,
        cx: &App,
    ) -> Task<Vec<(PathBuf, StaleIndexOutcome)>> {
        let StoreState::Local(local) = &self.state else {
            return Task::ready(Vec::new());
        };
        let Some(index_root) = local.index_root.clone() else {
            return Task::ready(Vec::new());
        };
        let mut keep = local
            .worktrees
            .values()
            .filter_map(|entry| Some(entry.index.as_ref()?.directory().to_path_buf()))
            .collect::<Vec<_>>();
        // A worktree whose index is still being opened (or failed to open) has none above,
        // and its directory is named after its resolved root, as `run_job` names it.
        let roots = local
            .worktrees
            .values()
            .filter_map(|entry| Some(entry.worktree.upgrade()?.read(cx).abs_path()))
            .collect::<Vec<_>>();
        let fs = local.fs.clone();
        cx.background_spawn(async move {
            for root in roots {
                // Resolved the way `WorktreeFileSource::canonicalize` resolves it, which on
                // Windows differs from `RealFs::canonicalize`.
                let canonical_root = if fs.is_fake() {
                    fs.canonicalize(&root).await
                } else {
                    std::fs::canonicalize(&root).map_err(anyhow::Error::from)
                };
                match canonical_root {
                    Ok(canonical_root) => {
                        keep.push(Self::index_directory(&index_root, &canonical_root))
                    }
                    Err(error) => log::info!(
                        "symbol_index: resolving {} to keep its index: {error:#}",
                        root.display()
                    ),
                }
            }
            symbol_index::remove_stale_indexes(&index_root, now, UNUSED_INDEX_AGE, &keep)
        })
    }

    pub fn remote(
        worktree_store: Entity<WorktreeStore>,
        client: AnyProtoClient,
        project_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.subscribe(&worktree_store, Self::on_worktree_store_event)];
        let mut this = Self {
            worktree_store,
            state: StoreState::Remote(RemoteStore {
                client,
                project_id,
                support: RemoteSupport::Unknown,
                statuses: HashMap::default(),
                _probe: None,
            }),
            scratch: None,
            _subscriptions: subscriptions,
        };
        this.probe_remote(cx);
        this
    }

    /// A project whose files no index here or on its host can answer for.
    pub fn unavailable(
        worktree_store: Entity<WorktreeStore>,
        reason: SymbolIndexUnavailable,
    ) -> Self {
        Self {
            worktree_store,
            state: StoreState::Unavailable(reason),
            scratch: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Asks a remote server for its statuses again after the connection came back: a server
    /// that was restarted meanwhile, or one that did not answer before, sends none by itself.
    pub fn reconnected(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, StoreState::Remote(_)) {
            self.probe_remote(cx);
        }
    }

    /// Sends status changes to the client of a remote server.
    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient, _: &mut Context<Self>) {
        if let StoreState::Local(local) = &mut self.state {
            local.downstream = Some((client, project_id));
        }
    }

    pub fn status(&self, worktree_id: WorktreeId) -> SymbolIndexStatus {
        match &self.state {
            StoreState::Local(local) => local
                .worktrees
                .get(&worktree_id)
                .map_or(SymbolIndexStatus::Disabled, |entry| entry.status.clone()),
            StoreState::Remote(remote) => match remote.support {
                RemoteSupport::Unsupported => SymbolIndexStatus::Unavailable {
                    reason: SymbolIndexUnavailable::UnsupportedHost,
                },
                RemoteSupport::Unknown => remote
                    .statuses
                    .get(&worktree_id)
                    .cloned()
                    .unwrap_or(SymbolIndexStatus::Scanning),
                RemoteSupport::Supported => remote
                    .statuses
                    .get(&worktree_id)
                    .cloned()
                    .unwrap_or(SymbolIndexStatus::Disabled),
            },
            StoreState::Unavailable(reason) => SymbolIndexStatus::Unavailable {
                reason: reason.clone(),
            },
        }
    }

    /// The status of every visible worktree's index.
    pub fn statuses(&self, cx: &App) -> Vec<(WorktreeId, SymbolIndexStatus)> {
        self.worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| {
                let worktree_id = worktree.read(cx).id();
                (worktree_id, self.status(worktree_id))
            })
            .collect()
    }

    /// Whether any worktree has an index that can answer (possibly while it is
    /// still being built).
    pub fn has_index(&self) -> bool {
        match &self.state {
            StoreState::Local(local) => local.worktrees.values().any(|entry| entry.index.is_some()),
            StoreState::Remote(remote) => {
                remote.support != RemoteSupport::Unsupported
                    && remote.statuses.values().any(|status| {
                        matches!(
                            status,
                            SymbolIndexStatus::Ready { .. } | SymbolIndexStatus::Indexing { .. }
                        )
                    })
            }
            StoreState::Unavailable(_) => false,
        }
    }

    /// Definitions and declarations named exactly `name`, definitions first.
    /// Of `static` ones, those in `near`'s file come first and those in other
    /// source files are left out, since they cannot be what `near` refers to;
    /// headers are kept, as their `static` functions are everyone's.
    pub fn definitions(
        &mut self,
        name: &str,
        near: Option<ProjectPath>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<IndexedDefinition>>> {
        match &self.state {
            StoreState::Local(local) => {
                let indexes = local.open_indexes();
                let name = name.to_string();
                let lookup = cx.background_spawn(async move {
                    let mut found = Vec::new();
                    for (worktree_id, index) in indexes {
                        found.extend(
                            index
                                .definitions(&name)
                                .into_iter()
                                .map(|definition| (worktree_id, definition)),
                        );
                    }
                    found
                });
                cx.spawn(async move |this, cx| {
                    let found = lookup.await;
                    this.update(cx, |this, cx| {
                        order_definitions(found, near.as_ref())
                            .into_iter()
                            .filter_map(|(worktree_id, definition)| {
                                this.convert_definition(worktree_id, definition, cx)
                            })
                            .take(MAX_DEFINITIONS)
                            .collect()
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some(client) = remote.client_for_queries() else {
                    return Task::ready(Ok(Vec::new()));
                };
                let request = client.0.request(proto::SymbolIndexDefinitions {
                    project_id: client.1,
                    name: name.to_string(),
                    near_worktree_id: near.as_ref().map(|near| near.worktree_id.to_proto()),
                    near_path: near
                        .as_ref()
                        .map(|near| near.path.as_unix_str().to_string()),
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    this.update(cx, |this, cx| {
                        response
                            .definitions
                            .into_iter()
                            .filter_map(definition_from_proto)
                            .filter(|definition| this.remote_result_shown(&definition.path, cx))
                            .take(MAX_DEFINITIONS)
                            .collect()
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(Vec::new())),
        }
    }

    /// Call sites of `name`, at most `limit` of them (and never more than
    /// [`MAX_CALLERS_LIMIT`]).
    pub fn callers(
        &mut self,
        name: &str,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<IndexedCallers>> {
        let limit = limit.min(MAX_CALLERS_LIMIT);
        match &self.state {
            StoreState::Local(local) => {
                let indexes = local.open_indexes();
                let name = name.to_string();
                let lookup = cx.background_spawn(async move {
                    let mut calls = Vec::new();
                    let mut truncated = false;
                    for (worktree_id, index) in indexes {
                        let result = index.callers(&name, limit);
                        truncated |= result.truncated;
                        calls.extend(result.calls.into_iter().map(|call| (worktree_id, call)));
                    }
                    if calls.len() > limit {
                        calls.truncate(limit);
                        truncated = true;
                    }
                    (calls, truncated)
                });
                cx.spawn(async move |this, cx| {
                    let (calls, truncated) = lookup.await;
                    this.update(cx, |this, cx| IndexedCallers {
                        calls: calls
                            .into_iter()
                            .filter_map(|(worktree_id, call)| {
                                let path = this.allowed_path(worktree_id, &call.path, cx)?;
                                Some(IndexedCallSite {
                                    range: range_to_points(call.range),
                                    callee: call.callee,
                                    kind: call.kind,
                                    in_macro_body: call.in_macro_body,
                                    caller: call.caller.and_then(|caller| {
                                        this.convert_definition(worktree_id, caller, cx)
                                    }),
                                    path,
                                })
                            })
                            .collect(),
                        truncated,
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some(client) = remote.client_for_queries() else {
                    return Task::ready(Ok(IndexedCallers::default()));
                };
                let request = client.0.request(proto::SymbolIndexCallers {
                    project_id: client.1,
                    name: name.to_string(),
                    limit: u32::try_from(limit).unwrap_or(u32::MAX),
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    this.update(cx, |this, cx| {
                        let mut calls = response
                            .calls
                            .into_iter()
                            .filter_map(call_site_from_proto)
                            .filter(|call| this.remote_result_shown(&call.path, cx))
                            .map(|mut call| {
                                call.caller = call
                                    .caller
                                    .filter(|caller| this.remote_result_shown(&caller.path, cx));
                                call
                            })
                            .collect::<Vec<_>>();
                        let mut truncated = response.truncated;
                        if calls.len() > limit {
                            calls.truncate(limit);
                            truncated = true;
                        }
                        IndexedCallers { calls, truncated }
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(IndexedCallers::default())),
        }
    }

    /// The calls made by the function (or macro) that contains `position` in
    /// `path`. `None` when no indexed function contains it.
    pub fn callees(
        &mut self,
        path: ProjectPath,
        position: Point,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<IndexedCallees>>> {
        match &self.state {
            StoreState::Local(local) => {
                let Some(index) = local
                    .worktrees
                    .get(&path.worktree_id)
                    .and_then(|entry| entry.index.clone())
                else {
                    return Task::ready(Ok(None));
                };
                let file = path.path.as_unix_str().to_string();
                let worktree_id = path.worktree_id;
                let lookup = cx.background_spawn(async move {
                    // An update between the two lookups makes the id stale;
                    // looking the function up again finds its new one.
                    for _ in 0..2 {
                        let Some(function) = enclosing_function(&index, &file, position) else {
                            return None;
                        };
                        if let Some(callees) = index.callees(function.id) {
                            return Some((function, callees));
                        }
                    }
                    None
                });
                cx.spawn(async move |this, cx| {
                    let Some((function, callees)) = lookup.await else {
                        return Ok(None);
                    };
                    this.update(cx, |this, cx| {
                        let Some(function) = this.convert_definition(worktree_id, function, cx)
                        else {
                            return None;
                        };
                        let callees = callees
                            .into_iter()
                            .filter_map(|callee| {
                                Some(IndexedCalleeSite {
                                    path: this.allowed_path(worktree_id, &callee.path, cx)?,
                                    range: range_to_points(callee.range),
                                    callee: callee.callee,
                                    kind: callee.kind,
                                    in_macro_body: callee.in_macro_body,
                                    target: callee.target,
                                })
                            })
                            .collect();
                        Some(IndexedCallees { function, callees })
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some(client) = remote.client_for_queries() else {
                    return Task::ready(Ok(None));
                };
                let request = client.0.request(proto::SymbolIndexCallees {
                    project_id: client.1,
                    worktree_id: path.worktree_id.to_proto(),
                    path: path.path.as_unix_str().to_string(),
                    row: position.row,
                    column: position.column,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    let Some(function) = response.function.and_then(definition_from_proto) else {
                        return Ok(None);
                    };
                    this.update(cx, |this, cx| {
                        if !this.remote_result_shown(&function.path, cx) {
                            return None;
                        }
                        Some(IndexedCallees {
                            function,
                            callees: response
                                .callees
                                .into_iter()
                                .filter_map(callee_site_from_proto)
                                .filter(|callee| this.remote_result_shown(&callee.path, cx))
                                .collect(),
                        })
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(None)),
        }
    }

    /// Functions stored into members named `name` (`.name = function` or
    /// `pointer->name = function`), or, with `by_function`, the members that
    /// the function `name` is stored into.
    pub fn assignments(
        &mut self,
        name: &str,
        by_function: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<IndexedAssignment>>> {
        match &self.state {
            StoreState::Local(local) => {
                let indexes = local.open_indexes();
                let name = name.to_string();
                let lookup = cx.background_spawn(async move {
                    let mut found = Vec::new();
                    for (worktree_id, index) in indexes {
                        let assignments = if by_function {
                            index.assignments_of_function(&name)
                        } else {
                            index.assignments(&name)
                        };
                        found.extend(
                            assignments
                                .into_iter()
                                .map(|assignment| (worktree_id, assignment)),
                        );
                    }
                    found
                });
                cx.spawn(async move |this, cx| {
                    let found = lookup.await;
                    this.update(cx, |this, cx| {
                        found
                            .into_iter()
                            .filter_map(|(worktree_id, assignment)| {
                                Some(IndexedAssignment {
                                    path: this.allowed_path(worktree_id, &assignment.path, cx)?,
                                    range: range_to_points(assignment.range),
                                    field: assignment.field,
                                    function: assignment.function,
                                    container: assignment.container,
                                    kind: assignment.kind,
                                })
                            })
                            .take(MAX_ASSIGNMENTS)
                            .collect()
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some(client) = remote.client_for_queries() else {
                    return Task::ready(Ok(Vec::new()));
                };
                let request = client.0.request(proto::SymbolIndexAssignments {
                    project_id: client.1,
                    name: name.to_string(),
                    by_function,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    this.update(cx, |this, cx| {
                        response
                            .assignments
                            .into_iter()
                            .filter_map(assignment_from_proto)
                            .filter(|assignment| this.remote_result_shown(&assignment.path, cx))
                            .take(MAX_ASSIGNMENTS)
                            .collect()
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(Vec::new())),
        }
    }

    /// Definitions whose names match `query`, best first.
    pub fn search(
        &mut self,
        query: &str,
        mode: SearchMode,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<IndexedSymbolMatch>>> {
        let limit = limit.min(MAX_SEARCH_LIMIT);
        match &self.state {
            StoreState::Local(local) => {
                let indexes = local.open_indexes();
                let query = query.to_string();
                let lookup = cx.background_spawn(async move {
                    let mut found = Vec::new();
                    for (worktree_id, index) in indexes {
                        found.extend(
                            index
                                .search(&query, mode, limit)
                                .into_iter()
                                .map(|found| (worktree_id, found)),
                        );
                    }
                    // Sorting is stable, so each index's own order breaks ties.
                    found.sort_by_key(|(_, found)| std::cmp::Reverse(found.score));
                    found.truncate(limit);
                    found
                });
                cx.spawn(async move |this, cx| {
                    let found = lookup.await;
                    this.update(cx, |this, cx| {
                        found
                            .into_iter()
                            .filter_map(|(worktree_id, found)| {
                                Some(IndexedSymbolMatch {
                                    definition: this.convert_definition(
                                        worktree_id,
                                        found.definition,
                                        cx,
                                    )?,
                                    score: found.score,
                                })
                            })
                            .collect()
                    })
                })
            }
            StoreState::Remote(remote) => {
                let Some(client) = remote.client_for_queries() else {
                    return Task::ready(Ok(Vec::new()));
                };
                let request = client.0.request(proto::SymbolIndexSearch {
                    project_id: client.1,
                    query: query.to_string(),
                    mode: search_mode_to_proto(mode) as i32,
                    limit: u32::try_from(limit).unwrap_or(u32::MAX),
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.spawn(async move |this, cx| {
                    let response = with_timeout(request, timeout).await?;
                    this.update(cx, |this, cx| {
                        response
                            .matches
                            .into_iter()
                            .filter_map(|found| {
                                Some(IndexedSymbolMatch {
                                    definition: definition_from_proto(found.definition?)?,
                                    score: found.score,
                                })
                            })
                            .filter(|found| this.remote_result_shown(&found.definition.path, cx))
                            .take(limit)
                            .collect()
                    })
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(Vec::new())),
        }
    }

    /// Opens the indexes again, including ones another window held before,
    /// and brings them up to date with the worktrees.
    pub fn rebuild(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        match &mut self.state {
            StoreState::Local(local) => {
                let worktree_ids = local.worktrees.keys().copied().collect::<Vec<_>>();
                for entry in local.worktrees.values_mut() {
                    entry.applied = None;
                    if matches!(entry.status, SymbolIndexStatus::Unavailable { .. }) {
                        close_index_in_background(entry.index.take());
                    }
                }
                for worktree_id in worktree_ids {
                    self.evaluate_worktree(worktree_id, cx);
                }
                Task::ready(Ok(()))
            }
            StoreState::Remote(remote) => {
                if remote.support == RemoteSupport::Unsupported {
                    remote.support = RemoteSupport::Unknown;
                    self.probe_remote(cx);
                    return Task::ready(Ok(()));
                }
                let request = remote.client.request(proto::RebuildSymbolIndex {
                    project_id: remote.project_id,
                });
                let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
                cx.background_spawn(async move {
                    with_timeout(request, timeout).await?;
                    Ok(())
                })
            }
            StoreState::Unavailable(_) => Task::ready(Ok(())),
        }
    }

    /// Where a worktree's index lives under `index_root`: a folder named by a
    /// hash of the worktree's path, so that reopening the folder finds it.
    pub fn index_directory(index_root: &Path, worktree_root: &Path) -> PathBuf {
        let digest = Sha256::digest(worktree_root.to_string_lossy().as_bytes());
        let mut name = String::with_capacity(32);
        for byte in digest.iter().take(16) {
            name.push_str(&format!("{byte:02x}"));
        }
        index_root.join(name)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_index_root_for_tests(
        &mut self,
        index_root: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        local.index_root = index_root;
        let worktree_ids = local.worktrees.keys().copied().collect::<Vec<_>>();
        for entry in local.worktrees.values_mut() {
            entry.applied = None;
        }
        for worktree_id in worktree_ids {
            self.evaluate_worktree(worktree_id, cx);
        }
    }

    /// Answers for `worktree_id` from `index`, built by the test, instead of
    /// indexing the worktree. File system changes are not followed.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_index_for_tests(
        &mut self,
        worktree_id: WorktreeId,
        index: Arc<SymbolIndex>,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return;
        };
        let root = worktree.read(cx).abs_path();
        let worktree = worktree.downgrade();
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let stats = stats_from_index(&index.stats());
        local.worktrees.insert(
            worktree_id,
            WorktreeIndex {
                worktree,
                root,
                status: SymbolIndexStatus::Ready { stats },
                index: Some(index),
                applied: None,
                initial_scan_done: false,
                job_running: false,
                pending: PendingWork::default(),
                cancel: Arc::new(AtomicBool::new(false)),
                _scan: None,
                _debounce: None,
                _job: None,
                _save: None,
            },
        );
        self.status_changed(worktree_id, cx);
    }

    /// Reports `status` for `worktree_id` (`Indexing` while an index inserted by
    /// [`Self::insert_index_for_tests`] keeps answering, say) and emits
    /// [`SymbolIndexStoreEvent::StatusChanged`], until the store's own work sets another.
    /// A local store only takes a worktree it knows; a client of a remote server takes any.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_status_for_tests(
        &mut self,
        worktree_id: WorktreeId,
        status: SymbolIndexStatus,
        cx: &mut Context<Self>,
    ) {
        match &mut self.state {
            StoreState::Local(local) => {
                let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
                    return;
                };
                entry.status = status;
            }
            StoreState::Remote(remote) => {
                remote.statuses.insert(worktree_id, status);
            }
            StoreState::Unavailable(_) => return,
        }
        self.status_changed(worktree_id, cx);
    }

    fn on_worktree_store_event(
        &mut self,
        _: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => {
                if matches!(self.state, StoreState::Local(_)) {
                    self.add_local_worktree(worktree, cx);
                }
            }
            WorktreeStoreEvent::WorktreeRemoved(_, worktree_id) => {
                let removed = match &mut self.state {
                    StoreState::Local(local) => local.worktrees.remove(worktree_id).is_some(),
                    StoreState::Remote(remote) => remote.statuses.remove(worktree_id).is_some(),
                    StoreState::Unavailable(_) => false,
                };
                if removed {
                    self.send_status_downstream(*worktree_id, true, cx);
                    cx.emit(SymbolIndexStoreEvent::StatusChanged(*worktree_id));
                }
            }
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, changes) => {
                self.on_entries_changed(*worktree_id, changes, cx);
            }
            _ => {}
        }
    }

    fn on_buffer_store_event(
        &mut self,
        _: Entity<BufferStore>,
        event: &BufferStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferStoreEvent::BufferAdded(buffer) => self.watch_buffer_edits(buffer, cx),
            BufferStoreEvent::BufferDropped(buffer_id) => {
                if let StoreState::Local(local) = &mut self.state {
                    local.buffer_edits.remove(buffer_id);
                    local.buffer_subscriptions.remove(buffer_id);
                }
            }
            _ => {}
        }
    }

    fn watch_buffer_edits(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let buffer_id = buffer.read(cx).remote_id();
        let subscription = cx.subscribe(buffer, |this, buffer, event: &BufferEvent, cx| {
            if matches!(event, BufferEvent::Edited { .. })
                && let StoreState::Local(local) = &mut this.state
            {
                local.buffer_edit_count += 1;
                local
                    .buffer_edits
                    .insert(buffer.read(cx).remote_id(), local.buffer_edit_count);
            }
        });
        if let StoreState::Local(local) = &mut self.state {
            local.buffer_subscriptions.insert(buffer_id, subscription);
        }
    }

    fn add_local_worktree(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let (worktree_id, root, covered, scan_complete) = {
            let worktree = worktree.read(cx);
            let Some(local_worktree) = worktree.as_local() else {
                return;
            };
            (
                worktree.id(),
                worktree.abs_path(),
                // Single files and Zed's own hidden worktrees are not projects
                // anyone searches.
                worktree.is_visible() && !worktree.is_single_file(),
                local_worktree.scan_complete(),
            )
        };
        if local.worktrees.contains_key(&worktree_id) {
            return;
        }
        let scan = covered.then(|| {
            cx.spawn(async move |this, cx| {
                scan_complete.await;
                this.update(cx, |this, cx| {
                    if let StoreState::Local(local) = &mut this.state
                        && let Some(entry) = local.worktrees.get_mut(&worktree_id)
                    {
                        entry.initial_scan_done = true;
                        entry.applied = None;
                    }
                    this.evaluate_worktree(worktree_id, cx);
                })
                .log_err();
            })
        });
        local.worktrees.insert(
            worktree_id,
            WorktreeIndex {
                worktree: worktree.downgrade(),
                root,
                status: if covered {
                    SymbolIndexStatus::Scanning
                } else {
                    SymbolIndexStatus::Disabled
                },
                index: None,
                applied: None,
                initial_scan_done: false,
                job_running: false,
                pending: PendingWork::default(),
                cancel: Arc::new(AtomicBool::new(false)),
                _scan: scan,
                _debounce: None,
                _job: None,
                _save: None,
            },
        );
        if covered {
            // `"off"` shows as off at once rather than after the scan.
            self.evaluate_worktree(worktree_id, cx);
        }
        self.status_changed(worktree_id, cx);
    }

    fn settings_for(worktree: &Entity<Worktree>, cx: &App) -> AppliedSettings {
        let worktree_id = worktree.read(cx).id();
        let location = SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        };
        let symbol_index = ProjectSettings::get(Some(location), cx).symbol_index;
        AppliedSettings {
            enabled: symbol_index.enabled,
            include_ignored: symbol_index.include_ignored,
            rules: Arc::new(PathRules::for_worktree(worktree_id, cx)),
        }
    }

    fn on_settings_changed(&mut self, cx: &mut Context<Self>) {
        let StoreState::Local(local) = &self.state else {
            return;
        };
        let worktree_ids = local.worktrees.keys().copied().collect::<Vec<_>>();
        for worktree_id in worktree_ids {
            self.evaluate_worktree(worktree_id, cx);
        }
    }

    /// Starts, stops or refreshes a worktree's index for the current settings.
    fn evaluate_worktree(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let has_index_root = local.index_root.is_some();
        let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if entry._scan.is_none() && !entry.initial_scan_done {
            // A worktree the index does not cover.
            return;
        }
        let Some(worktree) = entry.worktree.upgrade() else {
            return;
        };
        let settings = Self::settings_for(&worktree, cx);
        if entry.applied.as_ref() == Some(&settings) {
            return;
        }
        let enabled = settings.enabled != SymbolIndexEnabled::Off && has_index_root;
        entry.applied = Some(settings);
        if !enabled {
            entry.cancel.store(true, Ordering::SeqCst);
            entry.cancel = Arc::new(AtomicBool::new(false));
            close_index_in_background(entry.index.take());
            entry.pending = PendingWork::default();
            entry._debounce = None;
            entry._job = None;
            entry.job_running = false;
            self.set_status(worktree_id, SymbolIndexStatus::Disabled, cx);
            return;
        }
        if !entry.initial_scan_done {
            return;
        }
        entry.pending.full = true;
        self.start_job(worktree_id, cx);
    }

    fn on_entries_changed(
        &mut self,
        worktree_id: WorktreeId,
        changes: &UpdatedEntriesSet,
        cx: &mut Context<Self>,
    ) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
            return;
        };
        let enabled = entry
            .applied
            .as_ref()
            .is_some_and(|applied| applied.enabled != SymbolIndexEnabled::Off)
            && local.index_root.is_some();
        let in_use = matches!(
            entry.status,
            SymbolIndexStatus::Unavailable {
                reason: SymbolIndexUnavailable::InUseByAnotherWindow
            }
        );
        if !entry.initial_scan_done || !enabled || in_use {
            return;
        }
        let mut relevant = false;
        for (path, _, change) in changes.iter() {
            if !is_indexable_path(path.as_unix_str()) {
                continue;
            }
            relevant = true;
            match change {
                PathChange::Removed => {
                    entry.pending.changed.remove(path);
                    entry.pending.removed.insert(path.clone());
                }
                PathChange::Added
                | PathChange::Updated
                | PathChange::AddedOrUpdated
                | PathChange::Loaded => {
                    entry.pending.removed.remove(path);
                    entry.pending.changed.insert(path.clone());
                }
            }
        }
        if !relevant {
            return;
        }
        entry._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHANGE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.start_job(worktree_id, cx))
                .log_err();
        }));
    }

    fn start_job(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let fs = local.fs.clone();
        let Some(index_root) = local.index_root.clone() else {
            return;
        };
        let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if entry.job_running || entry.pending.is_empty() {
            return;
        }
        let Some(worktree) = entry.worktree.upgrade() else {
            return;
        };
        let Some(applied) = entry.applied.clone() else {
            return;
        };
        let pending = std::mem::take(&mut entry.pending);
        let kind = if pending.full
            || entry.index.is_none()
            || pending.changed.len() + pending.removed.len() > FULL_UPDATE_THRESHOLD
        {
            JobKind::Full
        } else {
            JobKind::Files {
                changed: pending.changed.into_iter().collect(),
                removed: pending.removed.into_iter().collect(),
            }
        };
        // A worktree whose folder was renamed or moved keeps its entries but
        // not its old path.
        entry.root = worktree.read(cx).abs_path();
        let job = Job {
            kind,
            snapshot: worktree.read(cx).snapshot(),
            include_ignored: applied.include_ignored,
            auto: applied.enabled == SymbolIndexEnabled::Auto,
            index: entry.index.clone(),
            index_root,
            source: WorktreeFileSource {
                read_directly: !fs.is_fake(),
                fs,
                root: entry.root.clone(),
                canonical_root: None,
                rules: applied.rules,
            },
            cancel: entry.cancel.clone(),
        };
        let (sender, mut receiver) = mpsc::unbounded();
        let spawned = std::thread::Builder::new()
            .name("symbol-index-update".to_string())
            .spawn(move || {
                let outcome = run_job(job, &sender);
                // The receiver is gone when the worktree was closed meanwhile.
                sender.unbounded_send(JobMessage::Finished(outcome)).ok();
            });
        if let Err(error) = spawned {
            log::error!("symbol_index: cannot start the update thread: {error}");
            self.set_status(
                worktree_id,
                SymbolIndexStatus::Unavailable {
                    reason: SymbolIndexUnavailable::Error(error.to_string().into()),
                },
                cx,
            );
            return;
        }
        entry.job_running = true;
        entry._save = None;
        let is_first_build = entry.index.is_none();
        entry._job = Some(cx.spawn(async move |this, cx| {
            while let Some(message) = receiver.next().await {
                let finished = matches!(message, JobMessage::Finished(_));
                let updated = this.update(cx, |this, cx| {
                    this.on_job_message(worktree_id, message, is_first_build, cx)
                });
                if finished || updated.is_err() {
                    break;
                }
            }
        }));
        if is_first_build {
            self.set_status(
                worktree_id,
                SymbolIndexStatus::Indexing { done: 0, total: 0 },
                cx,
            );
        }
    }

    fn on_job_message(
        &mut self,
        worktree_id: WorktreeId,
        message: JobMessage,
        is_first_build: bool,
        cx: &mut Context<Self>,
    ) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
            return;
        };
        match message {
            JobMessage::Opened(index) => {
                entry.index = Some(index);
            }
            JobMessage::Progress { done, total } => {
                // Small updates from file events do not flash the status.
                if is_first_build || total > FULL_UPDATE_THRESHOLD {
                    self.set_status(worktree_id, SymbolIndexStatus::Indexing { done, total }, cx);
                }
            }
            JobMessage::Finished(outcome) => {
                entry.job_running = false;
                entry._job = None;
                let status = match outcome {
                    JobOutcome::Updated(stats) => Some(SymbolIndexStatus::Ready { stats }),
                    JobOutcome::NothingToIndex => {
                        entry.index = None;
                        Some(SymbolIndexStatus::Disabled)
                    }
                    JobOutcome::Cancelled => None,
                    JobOutcome::OpenFailed(reason) => {
                        entry.index = None;
                        entry.pending = PendingWork::default();
                        Some(SymbolIndexStatus::Unavailable { reason })
                    }
                    JobOutcome::UpdateFailed(error) => Some(SymbolIndexStatus::Unavailable {
                        reason: SymbolIndexUnavailable::Error(error.into()),
                    }),
                };
                if let Some(status) = status {
                    self.set_status(worktree_id, status, cx);
                }
                self.start_job(worktree_id, cx);
                self.save_when_idle(worktree_id, cx);
            }
        }
    }

    /// Updates write the index's manifest at most every few seconds, so once they stop, what
    /// the last ones committed is written after a while (unless another update starts first).
    fn save_when_idle(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let StoreState::Local(local) = &mut self.state else {
            return;
        };
        let Some(entry) = local.worktrees.get_mut(&worktree_id) else {
            return;
        };
        if entry.job_running {
            return;
        }
        // Weak, so that a closed index lets go of its directory without waiting for this.
        let Some(index) = entry.index.as_ref().map(Arc::downgrade) else {
            return;
        };
        entry._save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(symbol_index::MANIFEST_WRITE_INTERVAL)
                .await;
            let Some(index) = index.upgrade() else {
                return;
            };
            let saved = cx.background_spawn(async move { index.save() }).await;
            if let Err(error) = saved {
                log::warn!("symbol_index: saving the index: {error:#}");
            }
        }));
    }

    fn set_status(
        &mut self,
        worktree_id: WorktreeId,
        status: SymbolIndexStatus,
        cx: &mut Context<Self>,
    ) {
        let changed = match &mut self.state {
            StoreState::Local(local) => match local.worktrees.get_mut(&worktree_id) {
                Some(entry) if entry.status != status => {
                    entry.status = status;
                    true
                }
                _ => false,
            },
            StoreState::Remote(remote) => {
                remote.statuses.insert(worktree_id, status.clone()) != Some(status)
            }
            StoreState::Unavailable(_) => false,
        };
        if changed {
            self.status_changed(worktree_id, cx);
        }
    }

    fn status_changed(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        self.send_status_downstream(worktree_id, false, cx);
        cx.emit(SymbolIndexStoreEvent::StatusChanged(worktree_id));
    }

    fn send_status_downstream(&self, worktree_id: WorktreeId, removed: bool, cx: &App) {
        let StoreState::Local(local) = &self.state else {
            return;
        };
        if status_pushes_are_withheld(cx) {
            return;
        }
        let Some((client, project_id)) = &local.downstream else {
            return;
        };
        let status = if removed {
            proto::SymbolIndexWorktreeStatus {
                worktree_id: worktree_id.to_proto(),
                removed: true,
                ..Default::default()
            }
        } else {
            status_to_proto(worktree_id, &self.status(worktree_id))
        };
        client
            .send(proto::UpdateSymbolIndexStatus {
                project_id: *project_id,
                statuses: vec![status],
            })
            .log_err();
    }

    fn probe_remote(&mut self, cx: &mut Context<Self>) {
        let StoreState::Remote(remote) = &mut self.state else {
            return;
        };
        let request = remote.client.request(proto::GetSymbolIndexStatus {
            project_id: remote.project_id,
        });
        let timeout = cx.background_executor().timer(REMOTE_REQUEST_TIMEOUT);
        remote._probe = Some(cx.spawn(async move |this, cx| {
            let response = with_timeout(request, timeout).await;
            this.update(cx, |this, cx| {
                let StoreState::Remote(remote) = &mut this.state else {
                    return;
                };
                let changed_ids = match response {
                    Ok(response) => {
                        remote.support = RemoteSupport::Supported;
                        let mut changed = Vec::new();
                        for status in response.statuses {
                            let worktree_id = WorktreeId::from_proto(status.worktree_id);
                            remote
                                .statuses
                                .insert(worktree_id, status_from_proto(&status));
                            changed.push(worktree_id);
                        }
                        changed
                    }
                    // A server that answered before is the same build after a reconnect;
                    // one slow answer then does not turn its index off for the session.
                    Err(error) if remote.support == RemoteSupport::Supported => {
                        log::warn!(
                            "symbol_index: the remote server did not answer for its symbol \
                             index after reconnecting: {error:#}"
                        );
                        Vec::new()
                    }
                    Err(error) => {
                        log::info!(
                            "symbol_index: the remote server does not answer for its symbol \
                             index, so only language servers are asked: {error:#}"
                        );
                        remote.support = RemoteSupport::Unsupported;
                        remote.statuses.keys().copied().collect()
                    }
                };
                for worktree_id in changed_ids {
                    cx.emit(SymbolIndexStoreEvent::StatusChanged(worktree_id));
                }
                let visible = this
                    .worktree_store
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).id())
                    .collect::<Vec<_>>();
                for worktree_id in visible {
                    cx.emit(SymbolIndexStoreEvent::StatusChanged(worktree_id));
                }
            })
            .log_err();
        }));
    }

    /// The path of a result, if the current settings still let it be shown:
    /// a file excluded, made private or ignored since the index read it is
    /// left out.
    fn allowed_path(&self, worktree_id: WorktreeId, path: &str, cx: &App) -> Option<ProjectPath> {
        let relative = RelPath::from_unix_str(path).ok()?;
        let worktree = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)?;
        let location = SettingsLocation {
            worktree_id,
            path: relative,
        };
        let include_ignored = ProjectSettings::get(Some(location), cx)
            .symbol_index
            .include_ignored;
        is_result_shown(
            WorktreeSettings::get(Some(location), cx),
            include_ignored,
            worktree.read(cx).entry_for_path(relative),
            relative,
        )
        .then(|| ProjectPath {
            worktree_id,
            path: relative.into_arc(),
        })
    }

    /// Whether this client's own settings let a result from the remote
    /// server be shown: the server judged with its settings, and a client
    /// holds its own `private_files` too (ADR-F65 decision 6).
    fn remote_result_shown(&self, path: &ProjectPath, cx: &App) -> bool {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
        else {
            return false;
        };
        let settings = WorktreeSettings::get(
            Some(SettingsLocation {
                worktree_id: path.worktree_id,
                path: &path.path,
            }),
            cx,
        );
        !settings.is_path_excluded(&path.path)
            && !settings.is_path_private(&path.path)
            && worktree
                .read(cx)
                .entry_for_path(&path.path)
                .is_none_or(|entry| !entry.is_private && !entry.is_external)
    }

    fn convert_definition(
        &self,
        worktree_id: WorktreeId,
        definition: Definition,
        cx: &App,
    ) -> Option<IndexedDefinition> {
        Some(IndexedDefinition {
            path: self.allowed_path(worktree_id, &definition.path, cx)?,
            name: definition.name,
            kind: definition.kind,
            is_static: definition.is_static,
            is_declaration: definition.is_declaration,
            language: definition.language,
            name_range: range_to_points(definition.name_range),
            item_range: range_to_points(definition.item_range),
        })
    }

    async fn handle_get_status(
        this: Entity<Self>,
        _: TypedEnvelope<proto::GetSymbolIndexStatus>,
        cx: AsyncApp,
    ) -> Result<proto::GetSymbolIndexStatusResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        Ok(
            this.read_with(&cx, |this, cx| proto::GetSymbolIndexStatusResponse {
                statuses: this
                    .statuses(cx)
                    .iter()
                    .map(|(worktree_id, status)| status_to_proto(*worktree_id, status))
                    .collect(),
            }),
        )
    }

    async fn handle_update_status(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateSymbolIndexStatus>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let StoreState::Remote(remote) = &mut this.state else {
                return;
            };
            remote.support = RemoteSupport::Supported;
            for status in envelope.payload.statuses {
                let worktree_id = WorktreeId::from_proto(status.worktree_id);
                if status.removed {
                    remote.statuses.remove(&worktree_id);
                } else {
                    remote
                        .statuses
                        .insert(worktree_id, status_from_proto(&status));
                }
                cx.emit(SymbolIndexStoreEvent::StatusChanged(worktree_id));
            }
        });
        Ok(())
    }

    async fn handle_definitions(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexDefinitions>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexDefinitionsResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let near = match (payload.near_worktree_id, payload.near_path) {
            (Some(worktree_id), Some(path)) => Some(ProjectPath {
                worktree_id: WorktreeId::from_proto(worktree_id),
                path: RelPath::from_unix_str(&path)?.into_arc(),
            }),
            _ => None,
        };
        let definitions = this
            .update(&mut cx, |this, cx| {
                this.definitions(&payload.name, near, cx)
            })
            .await?;
        Ok(proto::SymbolIndexDefinitionsResponse {
            definitions: definitions.iter().map(definition_to_proto).collect(),
        })
    }

    async fn handle_callers(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexCallers>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexCallersResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let limit = usize::try_from(payload.limit).unwrap_or(MAX_CALLERS_LIMIT);
        let callers = this
            .update(&mut cx, |this, cx| this.callers(&payload.name, limit, cx))
            .await?;
        Ok(proto::SymbolIndexCallersResponse {
            calls: callers.calls.iter().map(call_site_to_proto).collect(),
            truncated: callers.truncated,
        })
    }

    async fn handle_callees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexCallees>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexCalleesResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let path = ProjectPath {
            worktree_id: WorktreeId::from_proto(payload.worktree_id),
            path: RelPath::from_unix_str(&payload.path)?.into_arc(),
        };
        let position = Point::new(payload.row, payload.column);
        let callees = this
            .update(&mut cx, |this, cx| this.callees(path, position, cx))
            .await?;
        Ok(match callees {
            Some(callees) => proto::SymbolIndexCalleesResponse {
                function: Some(definition_to_proto(&callees.function)),
                callees: callees.callees.iter().map(callee_site_to_proto).collect(),
            },
            None => proto::SymbolIndexCalleesResponse::default(),
        })
    }

    async fn handle_assignments(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexAssignments>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexAssignmentsResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let assignments = this
            .update(&mut cx, |this, cx| {
                this.assignments(&payload.name, payload.by_function, cx)
            })
            .await?;
        Ok(proto::SymbolIndexAssignmentsResponse {
            assignments: assignments.iter().map(assignment_to_proto).collect(),
        })
    }

    async fn handle_search(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SymbolIndexSearch>,
        mut cx: AsyncApp,
    ) -> Result<proto::SymbolIndexSearchResponse> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        let payload = envelope.payload;
        let mode = search_mode_from_proto(payload.mode());
        let limit = usize::try_from(payload.limit).unwrap_or(MAX_SEARCH_LIMIT);
        let matches = this
            .update(&mut cx, |this, cx| {
                this.search(&payload.query, mode, limit, cx)
            })
            .await?;
        Ok(proto::SymbolIndexSearchResponse {
            matches: matches
                .iter()
                .map(|found| proto::SymbolIndexSymbolMatch {
                    definition: Some(definition_to_proto(&found.definition)),
                    score: found.score,
                })
                .collect(),
        })
    }

    async fn handle_rebuild(
        this: Entity<Self>,
        _: TypedEnvelope<proto::RebuildSymbolIndex>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let this = wait_if_imitating_an_older_server(this, &cx).await;
        this.update(&mut cx, |this, cx| this.rebuild(cx)).await?;
        Ok(proto::Ack {})
    }
}

impl LocalStore {
    fn open_indexes(&self) -> Vec<(WorktreeId, Arc<SymbolIndex>)> {
        let mut indexes = self
            .worktrees
            .iter()
            .filter_map(|(worktree_id, entry)| Some((*worktree_id, entry.index.clone()?)))
            .collect::<Vec<_>>();
        indexes.sort_by_key(|(worktree_id, _)| *worktree_id);
        indexes
    }
}

impl RemoteStore {
    /// Only a server known to have the index is asked: one from before it
    /// never answers, and the caller should not wait out the timeout while
    /// the first status request is still out.
    fn client_for_queries(&self) -> Option<(AnyProtoClient, u64)> {
        (self.support == RemoteSupport::Supported).then(|| (self.client.clone(), self.project_id))
    }
}

async fn with_timeout<T>(
    request: impl Future<Output = Result<T>>,
    timeout: impl Future<Output = ()>,
) -> Result<T> {
    futures::select_biased! {
        response = request.fuse() => response,
        _ = timeout.fuse() => Err(anyhow!("the remote server did not answer about its symbol index")),
    }
}

/// A server from before the index leaves its requests unanswered; tests
/// make this one do the same with [`LeaveSymbolIndexUnansweredForTests`].
/// The store is let go of while waiting, so that it can be released.
#[cfg(any(test, feature = "test-support"))]
async fn wait_if_imitating_an_older_server(
    this: Entity<SymbolIndexStore>,
    cx: &AsyncApp,
) -> Entity<SymbolIndexStore> {
    if cx.update(|cx| cx.has_global::<LeaveSymbolIndexUnansweredForTests>()) {
        drop(this);
        return futures::future::pending().await;
    }
    this
}

#[cfg(not(any(test, feature = "test-support")))]
async fn wait_if_imitating_an_older_server(
    this: Entity<SymbolIndexStore>,
    _: &AsyncApp,
) -> Entity<SymbolIndexStore> {
    this
}

#[cfg(any(test, feature = "test-support"))]
fn status_pushes_are_withheld(cx: &App) -> bool {
    cx.has_global::<LeaveSymbolIndexUnansweredForTests>()
}

#[cfg(not(any(test, feature = "test-support")))]
fn status_pushes_are_withheld(_: &App) -> bool {
    false
}

fn run_job(job: Job, sender: &mpsc::UnboundedSender<JobMessage>) -> JobOutcome {
    let Job {
        kind,
        snapshot,
        include_ignored,
        auto,
        index,
        index_root,
        mut source,
        cancel,
    } = job;
    // The resolved root also names the index, so that one folder reached by
    // different spellings (letter case on Windows, a link) has one index.
    let canonical_root = match source.canonicalize(&source.root) {
        Ok(root) => root,
        Err(error) => {
            log::error!(
                "symbol_index: resolving the worktree root {}: {error}",
                source.root.display()
            );
            return JobOutcome::UpdateFailed(format!("{error}"));
        }
    };
    let directory = SymbolIndexStore::index_directory(&index_root, &canonical_root);
    source.canonical_root = Some(canonical_root);
    let rules = source.rules.clone();
    let files = match &kind {
        JobKind::Full => Some(list_files(&snapshot, include_ignored, &rules)),
        JobKind::Files { .. } => None,
    };
    let index = match index {
        Some(index) => index,
        None => {
            if auto && files.as_ref().is_some_and(|files| files.is_empty()) {
                return JobOutcome::NothingToIndex;
            }
            if let Err(error) = prepare_index_root(&index_root) {
                log::error!("symbol_index: preparing {}: {error}", index_root.display());
                return JobOutcome::OpenFailed(SymbolIndexUnavailable::Error(
                    format!("{error}").into(),
                ));
            }
            match open_index_waiting_for_release(&directory, &cancel) {
                Ok(index) => {
                    let index = Arc::new(index);
                    sender
                        .unbounded_send(JobMessage::Opened(index.clone()))
                        .ok();
                    index
                }
                Err(reason) => return JobOutcome::OpenFailed(reason),
            }
        }
    };
    // Queries are answered from the open index meanwhile.
    let _turn = match &kind {
        JobKind::Full => match wait_for_full_update_turn(&cancel) {
            Some(turn) => Some(turn),
            None => return JobOutcome::Cancelled,
        },
        JobKind::Files { .. } => None,
    };
    if cancel.load(Ordering::SeqCst) {
        return JobOutcome::Cancelled;
    }
    let last_progress = parking_lot::Mutex::new(None::<Instant>);
    let progress = |progress: Progress| {
        if progress.phase != ProgressPhase::Indexing {
            return;
        }
        let mut last = last_progress.lock();
        let due = last.is_none_or(|last| last.elapsed() >= PROGRESS_INTERVAL);
        if due || progress.completed == progress.total {
            *last = Some(Instant::now());
            sender
                .unbounded_send(JobMessage::Progress {
                    done: progress.completed,
                    total: progress.total,
                })
                .ok();
        }
    };
    let result = match (kind, files) {
        (JobKind::Full, Some(files)) => index.update(&files, &source, &progress, &cancel),
        (JobKind::Files { changed, removed }, _) => {
            let mut changed_entries = Vec::new();
            let mut removed_paths = removed
                .iter()
                .map(|path| path.as_unix_str().to_string())
                .collect::<Vec<_>>();
            for path in &changed {
                match snapshot.entry_for_path(path) {
                    Some(entry) if is_indexable_entry(entry, include_ignored, &rules) => {
                        changed_entries.push(file_entry(entry));
                    }
                    // Gone again, or now private or ignored: the index drops it.
                    _ => removed_paths.push(path.as_unix_str().to_string()),
                }
            }
            let removed_paths = removed_paths.iter().map(String::as_str).collect::<Vec<_>>();
            index.update_files(&changed_entries, &removed_paths, &source, &cancel)
        }
        (JobKind::Full, None) => Err(anyhow!("a full update without a file list")),
    };
    match result {
        Ok(stats) if stats.cancelled => JobOutcome::Cancelled,
        Ok(_) => JobOutcome::Updated(stats_from_index(&index.stats())),
        Err(error) => {
            log::error!(
                "symbol_index: updating the index in {}: {error:#}",
                directory.display()
            );
            JobOutcome::UpdateFailed(format!("{error:#}"))
        }
    }
}

/// An update this process started before (the index was turned off and on
/// again, or its worktree reopened) may still hold the directory for as long
/// as it takes to notice it was cancelled, so "in use" is only believed once
/// it persists.
fn open_index_waiting_for_release(
    directory: &Path,
    cancel: &AtomicBool,
) -> std::result::Result<SymbolIndex, SymbolIndexUnavailable> {
    retry_open(cancel, IN_USE_RETRY_DELAY, || open_index(directory))
}

/// [`open_index_waiting_for_release`] with the opening and the wait given. Besides "in use",
/// one other failure is tried again: another process removing the index as unused lets go of
/// its lock a moment before its lock file and directory are gone, and an open caught in
/// between fails without the index being locked. The second try finds the directory gone (and
/// starts a new index) or whole.
fn retry_open<T>(
    cancel: &AtomicBool,
    delay: Duration,
    mut open: impl FnMut() -> std::result::Result<T, SymbolIndexUnavailable>,
) -> std::result::Result<T, SymbolIndexUnavailable> {
    let mut in_use_attempts = 0;
    let mut retried_failure = false;
    loop {
        match open() {
            Err(SymbolIndexUnavailable::InUseByAnotherWindow)
                if in_use_attempts < IN_USE_RETRIES && !cancel.load(Ordering::SeqCst) =>
            {
                in_use_attempts += 1;
                std::thread::sleep(delay);
            }
            Err(SymbolIndexUnavailable::Error(error))
                if !retried_failure && !cancel.load(Ordering::SeqCst) =>
            {
                log::info!("symbol_index: opening the index again after: {error}");
                retried_failure = true;
                std::thread::sleep(delay);
            }
            result => return result,
        }
    }
}

fn open_index(directory: &Path) -> std::result::Result<SymbolIndex, SymbolIndexUnavailable> {
    let parallelism = std::thread::available_parallelism()
        .map_or(2, |count| count.get())
        .div_ceil(2);
    let config = IndexConfig {
        // Leave half of the cores to the editor and the language servers.
        parallelism: Some(parallelism.max(1)),
        ..IndexConfig::default()
    };
    match SymbolIndex::open(directory, config) {
        Ok(index) => {
            if let Some(reason) = index.reset_reason() {
                log::info!(
                    "symbol_index: rebuilding the index in {}: {reason}",
                    directory.display()
                );
            }
            Ok(index)
        }
        Err(error) => {
            if index_is_locked(directory) {
                log::info!(
                    "symbol_index: the index in {} is open in another window",
                    directory.display()
                );
                Err(SymbolIndexUnavailable::InUseByAnotherWindow)
            } else {
                log::error!(
                    "symbol_index: opening the index in {}: {error:#}",
                    directory.display()
                );
                Err(SymbolIndexUnavailable::Error(format!("{error:#}").into()))
            }
        }
    }
}

/// Whether another process (or another project in this one) holds the index's
/// lock, which is what makes opening it fail with "in use".
fn index_is_locked(directory: &Path) -> bool {
    let Ok(lock_file) = std::fs::File::options()
        .read(true)
        .write(true)
        .open(directory.join("lock"))
    else {
        return false;
    };
    match lock_file.try_lock() {
        Ok(()) => {
            lock_file.unlock().log_err();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Full updates of every worktree (and every window) of this process take
/// turns, so that opening several large folders does not have each of them
/// parse with half of the cores at once.
static FULL_UPDATE_TURN: parking_lot::Mutex<()> = parking_lot::const_mutex(());

fn wait_for_full_update_turn(cancel: &AtomicBool) -> Option<parking_lot::MutexGuard<'static, ()>> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        if let Some(turn) = FULL_UPDATE_TURN.try_lock_for(Duration::from_millis(100)) {
            return Some(turn);
        }
    }
}

/// The index holds the names in the user's source files, so on a shared
/// machine (a remote server) only the user may enter its folder.
fn prepare_index_root(index_root: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(index_root)?;
        let mode = std::fs::metadata(index_root)?.permissions().mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(index_root, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(index_root)?;
    Ok(())
}

/// The check a result passes before it is shown, with the settings and the
/// worktree entry of the moment: the settings catch a file made private or
/// excluded (also by a folder's own settings) before the worktree has marked
/// its entry again, and the entry catches what only the worktree knows.
fn is_result_shown(
    settings: &WorktreeSettings,
    include_ignored: bool,
    entry: Option<&worktree::Entry>,
    path: &RelPath,
) -> bool {
    if settings.is_path_excluded(path) || settings.is_path_private(path) {
        return false;
    }
    entry.is_some_and(|entry| {
        entry.is_file()
            && !entry.is_private
            && !entry.is_external
            && (include_ignored || !entry.is_ignored)
    })
}

fn is_indexable_entry(entry: &worktree::Entry, include_ignored: bool, rules: &PathRules) -> bool {
    entry.is_file()
        && !entry.is_external
        && !entry.is_private
        && !entry.is_fifo
        && (include_ignored || !entry.is_ignored)
        && is_indexable_path(entry.path.as_unix_str())
        && !rules.hides(&entry.path)
}

fn file_entry(entry: &worktree::Entry) -> FileEntry {
    FileEntry {
        path: Arc::from(entry.path.as_unix_str()),
        mtime: entry.mtime.map(|mtime| mtime.timestamp_for_user()),
        size: entry.size,
    }
}

fn list_files(snapshot: &Snapshot, include_ignored: bool, rules: &PathRules) -> Vec<FileEntry> {
    snapshot
        .entries(include_ignored, 0)
        .filter(|entry| is_indexable_entry(entry, include_ignored, rules))
        .map(file_entry)
        .collect()
}

fn enclosing_function(index: &SymbolIndex, path: &str, position: Point) -> Option<Definition> {
    let symbols = index.file_symbols(path)?;
    let position = (position.row, position.column);
    symbols
        .definitions
        .into_iter()
        .filter(|definition| {
            let callable = (definition.kind.is_function_like() && !definition.is_declaration)
                || definition.kind == DefinitionKind::FunctionMacro;
            callable
                && position_key(definition.item_range.start) <= position
                && position <= position_key(definition.item_range.end)
        })
        .min_by_key(|definition| {
            definition
                .item_range
                .end
                .byte
                .saturating_sub(definition.item_range.start.byte)
        })
}

fn position_key(position: Position) -> (u32, u32) {
    (position.row, position.column)
}

fn is_header_path(path: &str) -> bool {
    let Some((_, extension)) = path.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension,
        "h" | "hh" | "hpp" | "hxx" | "h++" | "H" | "inl" | "ipp" | "tcc"
    )
}

fn order_definitions(
    definitions: Vec<(WorktreeId, Definition)>,
    near: Option<&ProjectPath>,
) -> Vec<(WorktreeId, Definition)> {
    let is_near_file = |worktree_id: WorktreeId, definition: &Definition| {
        near.is_some_and(|near| {
            near.worktree_id == worktree_id && near.path.as_unix_str() == definition.path.as_ref()
        })
    };
    let mut definitions = definitions
        .into_iter()
        .filter(|(worktree_id, definition)| {
            !definition.is_static
                || near.is_none()
                || is_near_file(*worktree_id, definition)
                || is_header_path(&definition.path)
        })
        .collect::<Vec<_>>();
    definitions.sort_by_key(|(worktree_id, definition)| {
        (
            definition.is_declaration,
            !is_near_file(*worktree_id, definition),
            near.is_some_and(|near| near.worktree_id != *worktree_id),
        )
    });
    definitions
}

fn stats_from_index(stats: &symbol_index::IndexStats) -> SymbolIndexStats {
    SymbolIndexStats {
        files: stats.files as u64,
        definitions: stats.definitions,
        calls: stats.calls,
        assignments: stats.assignments,
        files_with_syntax_errors: stats.files_with_syntax_errors as u64,
        disk_bytes: stats.disk_bytes,
    }
}

fn range_to_points(range: symbol_index::Range) -> Range<Point> {
    Point::new(range.start.row, range.start.column)..Point::new(range.end.row, range.end.column)
}

fn range_to_proto(range: &Range<Point>) -> proto::SymbolIndexRange {
    proto::SymbolIndexRange {
        start_row: range.start.row,
        start_column: range.start.column,
        end_row: range.end.row,
        end_column: range.end.column,
    }
}

fn range_from_proto(range: Option<proto::SymbolIndexRange>) -> Option<Range<Point>> {
    let range = range?;
    Some(
        Point::new(range.start_row, range.start_column)
            ..Point::new(range.end_row, range.end_column),
    )
}

fn location_to_proto(path: &ProjectPath, range: &Range<Point>) -> proto::SymbolIndexLocation {
    proto::SymbolIndexLocation {
        worktree_id: path.worktree_id.to_proto(),
        path: path.path.as_unix_str().to_string(),
        range: Some(range_to_proto(range)),
    }
}

fn location_from_proto(
    location: Option<proto::SymbolIndexLocation>,
) -> Option<(ProjectPath, Range<Point>)> {
    let location = location?;
    let path = RelPath::from_unix_str(&location.path).log_err()?.into_arc();
    Some((
        ProjectPath {
            worktree_id: WorktreeId::from_proto(location.worktree_id),
            path,
        },
        range_from_proto(location.range)?,
    ))
}

fn kind_to_proto(kind: DefinitionKind) -> proto::SymbolIndexDefinitionKind {
    use proto::SymbolIndexDefinitionKind as Kind;
    match kind {
        DefinitionKind::Function => Kind::SymbolIndexKindFunction,
        DefinitionKind::Method => Kind::SymbolIndexKindMethod,
        DefinitionKind::Macro => Kind::SymbolIndexKindMacro,
        DefinitionKind::FunctionMacro => Kind::SymbolIndexKindFunctionMacro,
        DefinitionKind::Struct => Kind::SymbolIndexKindStruct,
        DefinitionKind::Union => Kind::SymbolIndexKindUnion,
        DefinitionKind::Enum => Kind::SymbolIndexKindEnum,
        DefinitionKind::EnumConstant => Kind::SymbolIndexKindEnumConstant,
        DefinitionKind::Typedef => Kind::SymbolIndexKindTypedef,
        DefinitionKind::Variable => Kind::SymbolIndexKindVariable,
        DefinitionKind::Class => Kind::SymbolIndexKindClass,
    }
}

fn kind_from_proto(kind: proto::SymbolIndexDefinitionKind) -> Option<DefinitionKind> {
    use proto::SymbolIndexDefinitionKind as Kind;
    Some(match kind {
        Kind::SymbolIndexKindUnknown => return None,
        Kind::SymbolIndexKindFunction => DefinitionKind::Function,
        Kind::SymbolIndexKindMethod => DefinitionKind::Method,
        Kind::SymbolIndexKindMacro => DefinitionKind::Macro,
        Kind::SymbolIndexKindFunctionMacro => DefinitionKind::FunctionMacro,
        Kind::SymbolIndexKindStruct => DefinitionKind::Struct,
        Kind::SymbolIndexKindUnion => DefinitionKind::Union,
        Kind::SymbolIndexKindEnum => DefinitionKind::Enum,
        Kind::SymbolIndexKindEnumConstant => DefinitionKind::EnumConstant,
        Kind::SymbolIndexKindTypedef => DefinitionKind::Typedef,
        Kind::SymbolIndexKindVariable => DefinitionKind::Variable,
        Kind::SymbolIndexKindClass => DefinitionKind::Class,
    })
}

fn definition_to_proto(definition: &IndexedDefinition) -> proto::SymbolIndexDefinition {
    proto::SymbolIndexDefinition {
        name: definition.name.clone(),
        kind: kind_to_proto(definition.kind) as i32,
        is_static: definition.is_static,
        is_declaration: definition.is_declaration,
        is_cpp: definition.language == SourceLanguage::Cpp,
        location: Some(location_to_proto(&definition.path, &definition.name_range)),
        item_range: Some(range_to_proto(&definition.item_range)),
    }
}

fn definition_from_proto(definition: proto::SymbolIndexDefinition) -> Option<IndexedDefinition> {
    let kind = kind_from_proto(definition.kind())?;
    let (path, name_range) = location_from_proto(definition.location)?;
    Some(IndexedDefinition {
        name: definition.name,
        kind,
        is_static: definition.is_static,
        is_declaration: definition.is_declaration,
        language: if definition.is_cpp {
            SourceLanguage::Cpp
        } else {
            SourceLanguage::C
        },
        path,
        name_range,
        item_range: range_from_proto(definition.item_range)?,
    })
}

fn call_site_to_proto(call: &IndexedCallSite) -> proto::SymbolIndexCallSite {
    proto::SymbolIndexCallSite {
        location: Some(location_to_proto(&call.path, &call.range)),
        callee: call.callee.clone(),
        is_member_call: call.kind == CallKind::Member,
        in_macro_body: call.in_macro_body,
        caller: call.caller.as_ref().map(definition_to_proto),
    }
}

fn call_site_from_proto(call: proto::SymbolIndexCallSite) -> Option<IndexedCallSite> {
    let (path, range) = location_from_proto(call.location)?;
    Some(IndexedCallSite {
        path,
        range,
        callee: call.callee,
        kind: if call.is_member_call {
            CallKind::Member
        } else {
            CallKind::Direct
        },
        in_macro_body: call.in_macro_body,
        caller: call.caller.and_then(definition_from_proto),
    })
}

fn callee_site_to_proto(callee: &IndexedCalleeSite) -> proto::SymbolIndexCalleeSite {
    use proto::SymbolIndexCalleeTarget as Target;
    proto::SymbolIndexCalleeSite {
        location: Some(location_to_proto(&callee.path, &callee.range)),
        callee: callee.callee.clone(),
        is_member_call: callee.kind == CallKind::Member,
        in_macro_body: callee.in_macro_body,
        target: match callee.target {
            CalleeTarget::Function => Target::SymbolIndexTargetFunction,
            CalleeTarget::Macro => Target::SymbolIndexTargetMacro,
            CalleeTarget::FunctionOrMacro => Target::SymbolIndexTargetFunctionOrMacro,
            CalleeTarget::Indirect => Target::SymbolIndexTargetIndirect,
            CalleeTarget::Unresolved => Target::SymbolIndexTargetUnresolved,
        } as i32,
    }
}

fn callee_site_from_proto(callee: proto::SymbolIndexCalleeSite) -> Option<IndexedCalleeSite> {
    use proto::SymbolIndexCalleeTarget as Target;
    let target = match callee.target() {
        Target::SymbolIndexTargetFunction => CalleeTarget::Function,
        Target::SymbolIndexTargetMacro => CalleeTarget::Macro,
        Target::SymbolIndexTargetFunctionOrMacro => CalleeTarget::FunctionOrMacro,
        Target::SymbolIndexTargetIndirect => CalleeTarget::Indirect,
        Target::SymbolIndexTargetUnresolved => CalleeTarget::Unresolved,
    };
    let (path, range) = location_from_proto(callee.location)?;
    Some(IndexedCalleeSite {
        path,
        range,
        callee: callee.callee,
        kind: if callee.is_member_call {
            CallKind::Member
        } else {
            CallKind::Direct
        },
        in_macro_body: callee.in_macro_body,
        target,
    })
}

fn assignment_to_proto(assignment: &IndexedAssignment) -> proto::SymbolIndexAssignment {
    proto::SymbolIndexAssignment {
        location: Some(location_to_proto(&assignment.path, &assignment.range)),
        field: assignment.field.clone(),
        function: assignment.function.clone(),
        container: assignment.container.clone(),
        is_designated_initializer: assignment.kind == AssignmentKind::DesignatedInitializer,
    }
}

fn assignment_from_proto(assignment: proto::SymbolIndexAssignment) -> Option<IndexedAssignment> {
    let (path, range) = location_from_proto(assignment.location)?;
    Some(IndexedAssignment {
        path,
        range,
        field: assignment.field,
        function: assignment.function,
        container: assignment.container,
        kind: if assignment.is_designated_initializer {
            AssignmentKind::DesignatedInitializer
        } else {
            AssignmentKind::Assignment
        },
    })
}

fn search_mode_to_proto(mode: SearchMode) -> proto::SymbolIndexSearchMode {
    match mode {
        SearchMode::Prefix => proto::SymbolIndexSearchMode::SymbolIndexSearchPrefix,
        SearchMode::Substring => proto::SymbolIndexSearchMode::SymbolIndexSearchSubstring,
        SearchMode::Fuzzy => proto::SymbolIndexSearchMode::SymbolIndexSearchFuzzy,
    }
}

fn search_mode_from_proto(mode: proto::SymbolIndexSearchMode) -> SearchMode {
    match mode {
        proto::SymbolIndexSearchMode::SymbolIndexSearchPrefix => SearchMode::Prefix,
        proto::SymbolIndexSearchMode::SymbolIndexSearchSubstring => SearchMode::Substring,
        proto::SymbolIndexSearchMode::SymbolIndexSearchFuzzy => SearchMode::Fuzzy,
    }
}

fn status_to_proto(
    worktree_id: WorktreeId,
    status: &SymbolIndexStatus,
) -> proto::SymbolIndexWorktreeStatus {
    use proto::{SymbolIndexState as State, SymbolIndexUnavailableReason as Reason};
    let mut message = proto::SymbolIndexWorktreeStatus {
        worktree_id: worktree_id.to_proto(),
        ..Default::default()
    };
    match status {
        SymbolIndexStatus::Disabled => message.set_state(State::SymbolIndexDisabled),
        SymbolIndexStatus::Scanning => message.set_state(State::SymbolIndexScanning),
        SymbolIndexStatus::Indexing { done, total } => {
            message.set_state(State::SymbolIndexIndexing);
            message.done = *done as u64;
            message.total = *total as u64;
        }
        SymbolIndexStatus::Ready { stats } => {
            message.set_state(State::SymbolIndexReady);
            message.stats = Some(proto::SymbolIndexStats {
                files: stats.files,
                definitions: stats.definitions,
                calls: stats.calls,
                assignments: stats.assignments,
                files_with_syntax_errors: stats.files_with_syntax_errors,
                disk_bytes: stats.disk_bytes,
            });
        }
        SymbolIndexStatus::Unavailable { reason } => {
            message.set_state(State::SymbolIndexUnavailable);
            match reason {
                SymbolIndexUnavailable::InUseByAnotherWindow => {
                    message.set_reason(Reason::SymbolIndexUnavailableInUse)
                }
                SymbolIndexUnavailable::UnsupportedHost => {
                    message.set_reason(Reason::SymbolIndexUnavailableUnsupportedHost)
                }
                SymbolIndexUnavailable::Collaboration => {
                    message.set_reason(Reason::SymbolIndexUnavailableCollaboration)
                }
                SymbolIndexUnavailable::Error(error) => {
                    message.set_reason(Reason::SymbolIndexUnavailableError);
                    message.error = Some(error.to_string());
                }
            }
        }
    }
    message
}

fn status_from_proto(status: &proto::SymbolIndexWorktreeStatus) -> SymbolIndexStatus {
    use proto::{SymbolIndexState as State, SymbolIndexUnavailableReason as Reason};
    match status.state() {
        State::SymbolIndexDisabled => SymbolIndexStatus::Disabled,
        State::SymbolIndexScanning => SymbolIndexStatus::Scanning,
        State::SymbolIndexIndexing => SymbolIndexStatus::Indexing {
            done: usize::try_from(status.done).unwrap_or(usize::MAX),
            total: usize::try_from(status.total).unwrap_or(usize::MAX),
        },
        State::SymbolIndexReady => SymbolIndexStatus::Ready {
            stats: status
                .stats
                .as_ref()
                .map(|stats| SymbolIndexStats {
                    files: stats.files,
                    definitions: stats.definitions,
                    calls: stats.calls,
                    assignments: stats.assignments,
                    files_with_syntax_errors: stats.files_with_syntax_errors,
                    disk_bytes: stats.disk_bytes,
                })
                .unwrap_or_default(),
        },
        State::SymbolIndexUnavailable => SymbolIndexStatus::Unavailable {
            reason: match status.reason() {
                Reason::SymbolIndexUnavailableInUse => SymbolIndexUnavailable::InUseByAnotherWindow,
                Reason::SymbolIndexUnavailableUnsupportedHost => {
                    SymbolIndexUnavailable::UnsupportedHost
                }
                Reason::SymbolIndexUnavailableCollaboration => {
                    SymbolIndexUnavailable::Collaboration
                }
                Reason::SymbolIndexUnavailableError => {
                    SymbolIndexUnavailable::Error(status.error.clone().unwrap_or_default().into())
                }
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_private(patterns: &[&str], cx: &mut gpui::TestAppContext) -> WorktreeSettings {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            let mut settings = WorktreeSettings::get_global(cx).clone();
            settings.private_files =
                PathMatcher::new(patterns.iter().copied(), util::paths::PathStyle::Unix)
                    .expect("valid patterns");
            settings
        })
    }

    fn file_entry_at(path: &str) -> worktree::Entry {
        worktree::Entry {
            id: worktree::ProjectEntryId::from_proto(1),
            kind: worktree::EntryKind::File,
            path: RelPath::from_unix_str(path)
                .expect("a relative path")
                .into_arc(),
            inode: 1,
            mtime: None,
            canonical_path: None,
            is_ignored: false,
            is_hidden: false,
            is_always_included: false,
            is_external: false,
            is_private: false,
            size: 1,
            char_bag: Default::default(),
            is_fifo: false,
        }
    }

    #[gpui::test]
    fn test_a_result_is_hidden_by_either_check_alone(cx: &mut gpui::TestAppContext) {
        let path = RelPath::from_unix_str("src/key.c").expect("a relative path");
        let plain = settings_with_private(&[], cx);
        let private = settings_with_private(&["**/key.c"], cx);
        let clean_entry = file_entry_at("src/key.c");

        assert!(is_result_shown(&plain, false, Some(&clean_entry), path));

        // The settings alone: the entry has not been marked (yet, or ever,
        // for a folder's own settings).
        assert!(!is_result_shown(&private, false, Some(&clean_entry), path));

        // The entry alone: the settings say nothing, but the worktree (or,
        // on a client, the host) marked it private, or it is a link out.
        let marked_private = worktree::Entry {
            is_private: true,
            ..clean_entry.clone()
        };
        assert!(!is_result_shown(&plain, false, Some(&marked_private), path));
        let external = worktree::Entry {
            is_external: true,
            ..clean_entry.clone()
        };
        assert!(!is_result_shown(&plain, false, Some(&external), path));

        let ignored = worktree::Entry {
            is_ignored: true,
            ..clean_entry.clone()
        };
        assert!(!is_result_shown(&plain, false, Some(&ignored), path));
        assert!(is_result_shown(&plain, true, Some(&ignored), path));

        let directory = worktree::Entry {
            kind: worktree::EntryKind::Dir,
            ..clean_entry
        };
        assert!(!is_result_shown(&plain, false, Some(&directory), path));
        assert!(
            !is_result_shown(&plain, false, None, path),
            "a file the worktree no longer has is not shown"
        );
    }

    #[test]
    fn test_an_open_that_fails_unlocked_is_tried_once_more() {
        let cancel = AtomicBool::new(false);
        let failure = || SymbolIndexUnavailable::Error("removed meanwhile".into());
        let attempts = std::cell::Cell::new(0);
        let open_failing = |failures: usize| {
            attempts.set(0);
            retry_open(&cancel, Duration::ZERO, || {
                attempts.set(attempts.get() + 1);
                if attempts.get() <= failures {
                    Err(failure())
                } else {
                    Ok(attempts.get())
                }
            })
        };
        assert_eq!(open_failing(0), Ok(1));
        assert_eq!(
            open_failing(1),
            Ok(2),
            "the race with a removal is tried again"
        );
        assert_eq!(
            open_failing(2),
            Err(failure()),
            "a lasting failure is reported"
        );
        assert_eq!(attempts.get(), 2);

        // "In use" keeps its own, longer wait.
        attempts.set(0);
        let in_use = retry_open(&cancel, Duration::ZERO, || {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(SymbolIndexUnavailable::InUseByAnotherWindow)
        });
        assert_eq!(in_use, Err(SymbolIndexUnavailable::InUseByAnotherWindow));
        assert_eq!(attempts.get(), IN_USE_RETRIES + 1);

        // A cancelled open is not tried again.
        cancel.store(true, Ordering::SeqCst);
        assert_eq!(open_failing(1), Err(failure()));
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn test_path_rules_follow_the_deepest_folder_settings() {
        let matcher = |patterns: &[&str]| {
            PathMatcher::new(patterns.iter().copied(), util::paths::PathStyle::Unix)
                .expect("valid patterns")
        };
        let folder = |path: &str| RelPath::from_unix_str(path).expect("a path").into_arc();
        let rules = PathRules {
            global: (matcher(&["**/vendor"]), matcher(&["**/*.key.c"])),
            folders: vec![
                (folder("lib"), matcher(&[]), matcher(&["**/secret.c"])),
                (
                    folder("lib/inner"),
                    matcher(&["**/lib/inner/gen"]),
                    matcher(&[]),
                ),
            ],
        };
        let hides = |path: &str| rules.hides(RelPath::from_unix_str(path).expect("a path"));
        assert!(hides("vendor/a.c"), "the global exclusions");
        assert!(hides("a.key.c"), "the global private files");
        assert!(!hides("main.c"));
        assert!(hides("lib/secret.c"), "a folder's private files");
        assert!(
            hides("lib/inner/gen/a.c"),
            "the deepest folder's exclusions"
        );
        // The store resolves each folder's settings with its parents', so
        // only the deepest folder's are looked at, as the store does.
        assert!(!hides("lib/inner/secret.c"));
        assert!(!hides("secret.c"), "a folder's settings stay in the folder");
    }
}

/// Makes the remote server leave the symbol index requests unanswered and
/// send no status, like a server from before the symbol index.
#[cfg(any(test, feature = "test-support"))]
pub struct LeaveSymbolIndexUnansweredForTests;

#[cfg(any(test, feature = "test-support"))]
impl gpui::Global for LeaveSymbolIndexUnansweredForTests {}

/// An in-memory [`FileSource`] for building an index in tests.
#[cfg(any(test, feature = "test-support"))]
pub struct InMemoryFileSource(pub HashMap<String, Vec<u8>>);

#[cfg(any(test, feature = "test-support"))]
impl FileSource for InMemoryFileSource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.0
            .get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, path.to_string()))
    }
}

/// Builds an index of `files` (path relative to the worktree, contents) in
/// `directory`, for [`SymbolIndexStore::insert_index_for_tests`].
#[cfg(any(test, feature = "test-support"))]
pub fn build_index_for_tests(directory: &Path, files: &[(&str, &str)]) -> Result<Arc<SymbolIndex>> {
    use anyhow::Context as _;
    let index = SymbolIndex::open(directory, IndexConfig::default())?;
    let entries = files
        .iter()
        .map(|(path, contents)| FileEntry {
            path: Arc::from(*path),
            mtime: None,
            size: contents.len() as u64,
        })
        .collect::<Vec<_>>();
    let source = InMemoryFileSource(
        files
            .iter()
            .map(|(path, contents)| (path.to_string(), contents.as_bytes().to_vec()))
            .collect(),
    );
    index
        .update(&entries, &source, &|_| {}, &AtomicBool::new(false))
        .context("building a test index")?;
    Ok(Arc::new(index))
}
