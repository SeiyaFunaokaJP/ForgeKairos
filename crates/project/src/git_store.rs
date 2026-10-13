mod conflict_set;
pub mod diff_buffer_list;
pub mod git_traversal;
pub mod job_debug_queue;
pub mod pending_op;

use crate::{
    Project, ProjectEnvironment, ProjectItem, ProjectPath,
    buffer_store::{BufferStore, BufferStoreEvent},
    project_settings::ProjectSettings,
    trusted_worktrees::{
        PathTrust, TrustedWorktrees, TrustedWorktreesEvent, TrustedWorktreesStore,
    },
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use anyhow::{Context as _, Result, anyhow, bail};
use askpass::{AskPassDelegate, EncryptedPassword, IKnowWhatIAmDoingAndIHaveReadTheDocs};
use async_lock::Semaphore;
use buffer_diff::{
    BufferDiff, DiffHunk, DiffHunkSecondaryStatus, DiffOperations, PendingHunk, PendingSense,
};
use client::ProjectId;
use collections::HashMap;
pub use conflict_set::{ConflictRegion, ConflictSet, ConflictSetSnapshot, ConflictSetUpdate};
use fs::{Fs, RemoveOptions};
use futures::{
    FutureExt, SinkExt, Stream, StreamExt,
    channel::{
        mpsc,
        oneshot::{self, Canceled},
    },
    future::{self, BoxFuture, Shared},
    stream::{FuturesOrdered, FuturesUnordered},
};
use git::{
    BuildPermalinkParams, GitHostingProviderRegistry, Oid, RunHook,
    blame::Blame,
    commit_search::{COMMIT_SEARCH_RESULT_LIMIT, MergeFilter},
    parse_git_remote_url,
    repository::{
        Branch, BranchesScanResult, CommitData, CommitDetails, CommitFileStatus, CommitOptions,
        CreateWorktreeTarget, DiffStatType, DiffType, FetchOptions, FileHistoryChangedFileSets,
        GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY, GitCommandOutput, GitCommitTemplate, GitRepository,
        GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource, LooseRefCount,
        PatchSource, PushOptions, RefStorageScan, Remote, RemoteCommandOutput, RepoPath, ResetMode,
        SearchCommitArgs, SequencerKind, SequencerProbes, SequencerState, TAG_LIST_FORMAT,
        TagDistance, TagInfo, UpstreamTrackingStatus, Worktree as GitWorktree, delete_branch_flag,
        is_binary_content, sequencer_state_from_probes,
    },
    stash::{GitStash, StashEntry},
    status::{
        self, DiffStat, DiffTreeType, FileStatus, GitSummary, StatusCode, TrackedStatus, TreeDiff,
        TreeDiffStatus, UnmergedStatus, UnmergedStatusCode,
    },
};
use gpui::{
    App, AppContext, AsyncApp, BackgroundExecutor, Context, Entity, EntityId, EventEmitter,
    SharedString, Subscription, Task, TaskExt, WeakEntity,
};
use language::{
    Anchor, Buffer, BufferEvent, Capability, Language, LanguageRegistry, decode_text, encode_text,
    proto::{deserialize_version, serialize_version},
};
use parking_lot::Mutex;
use paths::{config_dir, home_dir};
use pending_op::{PendingOp, PendingOpId, PendingOps, PendingOpsSummary};
use postage::stream::Stream as _;
use rpc::{
    AnyProtoClient, TypedEnvelope,
    proto::{self, git_reset, split_repository_update},
};
use serde::Deserialize;
use settings::{GitDiffBaseSetting, Settings, SettingsLocation, SettingsStore, WorktreeId};
use smallvec::SmallVec;
use smol::future::yield_now;
use std::{
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeSet, HashSet, VecDeque, hash_map::Entry},
    future::Future,
    mem,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    str::FromStr,
    sync::{
        Arc,
        atomic::{self, AtomicBool, AtomicU64},
    },
    time::{Duration, Instant, SystemTime},
};
use sum_tree::{Edit, SumTree, TreeMap};
use task::Shell;
use text::{Bias, BufferId, OffsetRangeExt, Rope, ToOffset};
use util::{
    ResultExt, debug_panic,
    paths::{PathStyle, SanitizedPath},
    post_inc,
    rel_path::RelPath,
};
use worktree::{
    File, GitDirChange, PathChange, PathKey, PathProgress, PathSummary, PathTarget, ProjectEntryId,
    UpdatedGitRepositoriesSet, UpdatedGitRepository, Worktree, WorktreeSettings,
};
use zeroize::Zeroize;

pub struct GitStore {
    state: GitStoreState,
    project: Option<WeakEntity<Project>>,
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    parked_repositories: Vec<ParkedRepository>,
    diff_base: GitDiffBaseSetting,
    display_diffs: HashMap<RepositoryId, DisplayDiff>,
    worktree_ids: HashMap<RepositoryId, HashSet<WorktreeId>>,
    active_repo_id: Option<RepositoryId>,
    #[allow(clippy::type_complexity)]
    loading_diffs:
        HashMap<(BufferId, DiffKind), Shared<Task<Result<Entity<BufferDiff>, Arc<anyhow::Error>>>>>,
    diffs: HashMap<BufferId, Entity<BufferGitState>>,
    buffer_ids_by_index_text_buffer_id: HashMap<BufferId, BufferId>,
    shared_diffs: HashMap<proto::PeerId, HashMap<BufferId, SharedDiffs>>,
    commit_searches: HashMap<(proto::PeerId, u64), oneshot::Sender<()>>,
    blob_read_limiter: Arc<Semaphore>,
    /// ADR-F90: local worktrees whose first scan has not finished. A repository found while its
    /// worktree is in here was there when the user opened the folder.
    worktrees_in_initial_scan: HashSet<WorktreeId>,
    /// ADR-F90: the working directories of the repositories the user opened: found by a
    /// worktree's first scan, or made with [`Self::git_init`]. An AI agent's `git` tool runs only
    /// in these; a repository that appears later may be one the agent made from its sandbox,
    /// configuration and all.
    repositories_opened_by_user: Rc<RefCell<HashSet<Arc<Path>>>>,
    /// ADR-F93: for a client of a remote server, the host's connection, which the choices of
    /// its repositories' git accounts are kept under.
    remote_git_account_connection: Option<String>,
    _subscriptions: Vec<Subscription>,
}

const MIN_PARKED_REPOSITORY_DEPTH: usize = 2;

pub const MAX_CONCURRENT_BLOB_READS: usize = 16;

#[derive(Clone, Debug)]
pub struct ParkedRepository {
    worktree_id: WorktreeId,
    work_directory_abs_path: Arc<Path>,
    dot_git_abs_path: Arc<Path>,
    repository_dir_abs_path: Arc<Path>,
    common_dir_abs_path: Arc<Path>,
}

impl ParkedRepository {
    #[cfg(feature = "test-support")]
    pub fn work_directory_abs_path(&self) -> &Path {
        &self.work_directory_abs_path
    }
}

#[derive(Default)]
struct DisplayDiff {
    buffers: Option<(Entity<diff_buffer_list::DiffBufferList>, Subscription)>,
    refresh: Option<Task<()>>,
}

#[derive(Default)]
struct SharedDiffs {
    unstaged: Option<Entity<BufferDiff>>,
    uncommitted: Option<Entity<BufferDiff>>,
}

struct BufferGitState {
    unstaged_diff: Option<WeakEntity<BufferDiff>>,
    staged_diff: Option<(WeakEntity<BufferDiff>, Entity<Buffer>)>,
    uncommitted_diff: Option<WeakEntity<BufferDiff>>,
    oid_diffs: HashMap<Option<git::Oid>, WeakEntity<BufferDiff>>,
    conflict_set: Option<WeakEntity<ConflictSet>>,
    recalculate_diff_task: Option<Task<Result<()>>>,
    reparse_conflict_markers_task: Option<Task<Result<()>>>,
    language: Option<Arc<Language>>,
    language_registry: Option<Arc<LanguageRegistry>>,
    conflict_updated_futures: Vec<oneshot::Sender<()>>,
    recalculating_tx: postage::watch::Sender<bool>,

    /// These operation counts are used to ensure that head and index text
    /// values read from the git repository are up-to-date with any hunk staging
    /// operations that have been performed on the BufferDiff.
    ///
    /// The operation count is incremented immediately when the user initiates a
    /// hunk stage/unstage operation. Then, upon finishing writing the new index
    /// text do disk, the `operation count as of write` is updated to reflect
    /// the operation count that prompted the write.
    hunk_staging_operation_count: usize,
    hunk_staging_operation_count_as_of_write: usize,

    head_text: Option<Arc<str>>,
    index_text: Option<Arc<str>>,
    /// The optimistic, in-flight index state: the sole input to the index write,
    /// expressed relative to the currently-loaded index text (`I0`). Never shown
    /// to any view (views use per-diff pending hunks). Cleared when a
    /// recalculation settles.
    ///
    /// `I0` is immutable for the lifetime of any pending edit (the index text
    /// buffer is only fast-forwarded when a recalculation settles, which clears
    /// this state in the same update), so byte offsets stay valid for the whole
    /// window.
    pending_index_edits: Option<Vec<(Range<usize>, Arc<str>)>>,
    oid_texts: HashMap<git::Oid, Arc<str>>,
    head_text_buffer: WeakEntity<Buffer>,
    index_text_buffer: WeakEntity<Buffer>,
    index_text_buffer_language_enabled: bool,
    head_changed: bool,
    index_changed: bool,
    language_changed: bool,
}

fn pending_hunks(
    hunks: &[DiffHunk],
    version: &clock::Global,
    sense: PendingSense,
) -> Vec<PendingHunk> {
    hunks
        .iter()
        .map(|hunk| {
            PendingHunk::new(
                hunk.buffer_range.clone(),
                hunk.diff_base_byte_range.clone(),
                version.clone(),
                sense,
            )
        })
        .collect()
}

fn decode_git_text(bytes: Vec<u8>) -> Result<String> {
    Ok(decode_text(bytes)?.text)
}

#[derive(Debug)]
pub struct CommitDiff {
    pub files: Vec<CommitFile>,
    pub is_shallow_boundary: bool,
}

#[derive(Debug)]
pub struct CommitFile {
    pub path: RepoPath,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub is_binary: bool,
}

impl CommitFile {
    pub fn status(&self) -> CommitFileStatus {
        match (&self.old_text, &self.new_text) {
            (None, Some(_)) => CommitFileStatus::Added,
            (Some(_), None) => CommitFileStatus::Deleted,
            _ => CommitFileStatus::Modified,
        }
    }
}

fn decode_commit_diff(diff: git::repository::CommitDiff) -> CommitDiff {
    let files = diff
        .files
        .into_iter()
        .map(|file| {
            let git::repository::CommitFile {
                path,
                old_content,
                new_content,
                mut is_binary,
            } = file;

            if is_binary {
                return CommitFile {
                    path,
                    old_text: old_content.map(|_| String::new()),
                    new_text: new_content.map(|_| String::new()),
                    is_binary,
                };
            }

            let mut decode_content = |content: Option<Vec<u8>>| {
                content.map(|content| match decode_git_text(content) {
                    Ok(text) => text,
                    Err(error) => {
                        log::debug!(
                            "treating commit content for {} as binary after decoding failed: {error:#}",
                            path.as_unix_str()
                        );
                        is_binary = true;
                        String::new()
                    }
                })
            };
            let mut old_text = decode_content(old_content);
            let mut new_text = decode_content(new_content);
            if is_binary {
                old_text = old_text.map(|_| String::new());
                new_text = new_text.map(|_| String::new());
            }

            CommitFile {
                path,
                old_text,
                new_text,
                is_binary,
            }
        })
        .collect();
    CommitDiff {
        files,
        is_shallow_boundary: diff.is_shallow_boundary,
    }
}

#[derive(Clone, Debug)]
enum DiffBasesChange {
    SetIndex(Option<String>),
    SetHead(Option<String>),
    SetEach {
        index: Option<String>,
        head: Option<String>,
    },
    SetBoth(Option<String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DiffKind {
    Unstaged,
    Staged,
    Uncommitted,
    SinceOid(Option<git::Oid>),
}

struct GitDiffOperations {
    project: WeakEntity<Project>,
    kind: DiffKind,
}

impl DiffOperations for GitDiffOperations {
    fn supports_staging(&self) -> bool {
        matches!(self.kind, DiffKind::Unstaged | DiffKind::Uncommitted)
    }

    fn supports_unstaging(&self) -> bool {
        matches!(self.kind, DiffKind::Staged | DiffKind::Uncommitted)
    }

    fn supports_restore(&self) -> bool {
        matches!(self.kind, DiffKind::Unstaged | DiffKind::Uncommitted)
    }

    fn stage(
        &self,
        diff: Entity<BufferDiff>,
        buffer: Option<Entity<Buffer>>,
        buffer_ranges: Vec<Range<Anchor>>,
        cx: &mut App,
    ) {
        let result = self.project.update(cx, |project, cx| match self.kind {
            DiffKind::Unstaged => {
                let buffer = buffer.context("unstaged diff has no worktree buffer")?;
                project.stage_hunks(buffer, diff, buffer_ranges, cx)
            }
            DiffKind::Uncommitted => {
                let buffer = buffer.context("uncommitted diff has no worktree buffer")?;
                let secondary_diff = diff
                    .read(cx)
                    .secondary_diff()
                    .context("diff has no unstaged secondary")?;
                project.stage_hunks(buffer, secondary_diff, buffer_ranges, cx)
            }
            _ => Ok(()),
        });
        result.and_then(|result| result).log_err();
    }

    fn unstage(
        &self,
        diff: Entity<BufferDiff>,
        buffer: Option<Entity<Buffer>>,
        buffer_ranges: Vec<Range<Anchor>>,
        cx: &mut App,
    ) {
        let result = self.project.update(cx, |project, cx| match self.kind {
            DiffKind::Staged => project.unstage_staged_hunks(diff, buffer_ranges, cx),
            DiffKind::Uncommitted => {
                let buffer = buffer.context("uncommitted diff has no worktree buffer")?;
                project.unstage_uncommitted_hunks(buffer, diff, buffer_ranges, cx)
            }
            _ => Ok(()),
        });
        result.and_then(|result| result).log_err();
    }
}

struct IndexTextFile {
    path: Arc<RelPath>,
    full_path: PathBuf,
    path_style: PathStyle,
    file_name: String,
    worktree_id: WorktreeId,
    is_private: bool,
}

impl IndexTextFile {
    fn new(file: &dyn language::File, cx: &App) -> Self {
        Self {
            path: file.path().clone(),
            full_path: file.full_path(cx),
            path_style: file.path_style(cx),
            file_name: file.file_name(cx).to_string(),
            worktree_id: file.worktree_id(cx),
            is_private: file.is_private(),
        }
    }
}

impl language::File for IndexTextFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> language::DiskState {
        language::DiskState::Historic { was_deleted: false }
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.full_path.clone()
    }

    fn path_style(&self, _: &App) -> PathStyle {
        self.path_style
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        &self.file_name
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _: &App) -> rpc::proto::File {
        rpc::proto::File {
            worktree_id: self.worktree_id.to_proto(),
            entry_id: None,
            path: self.path.as_ref().as_unix_str().to_owned(),
            mtime: None,
            is_deleted: false,
            is_historic: true,
        }
    }

    fn is_private(&self) -> bool {
        self.is_private
    }
}

#[derive(Debug, Clone, Copy)]
pub enum GitAccess {
    /// Either:
    /// - the user owns `.git`
    /// - the user doesn't own `.git`, but has both of:
    ///   - OS-level read permissions
    ///   - the directory is marked as safe (git config safe.directory)
    Yes,

    /// The user is not the owner of `.git`, and one of the following is true:
    /// - the directory is not marked as safe (git config safe.directory)
    /// - the user does not have OS-level read permissions to `.git`
    No,
}

enum GitStoreState {
    Local {
        next_repository_id: Arc<AtomicU64>,
        downstream: Option<LocalDownstreamState>,
        project_environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        _fs_watches: Box<[Task<()>]>,
    },
    Remote {
        upstream_client: AnyProtoClient,
        upstream_project_id: u64,
        downstream: Option<(AnyProtoClient, ProjectId)>,
    },
}

enum DownstreamUpdate {
    UpdateRepository(RepositorySnapshot),
    RemoveRepository(RepositoryId),
}

struct LocalDownstreamState {
    client: AnyProtoClient,
    project_id: ProjectId,
    updates_tx: mpsc::UnboundedSender<DownstreamUpdate>,
    _task: Task<Result<()>>,
}

#[derive(Clone, Debug)]
pub struct GitStoreCheckpoint {
    checkpoints_by_work_dir_abs_path: HashMap<Arc<Path>, GitRepositoryCheckpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub repo_path: RepoPath,
    pub status: FileStatus,
    pub diff_stat: Option<DiffStat>,
    pub staged_diff_stat: Option<DiffStat>,
    pub unstaged_diff_stat: Option<DiffStat>,
}

impl StatusEntry {
    fn to_proto(&self) -> proto::StatusEntry {
        let simple_status = match self.status {
            FileStatus::Ignored | FileStatus::Untracked => proto::GitStatus::Added as i32,
            FileStatus::Unmerged { .. } => proto::GitStatus::Conflict as i32,
            FileStatus::Tracked(TrackedStatus {
                index_status,
                worktree_status,
            }) => tracked_status_to_proto(if worktree_status != StatusCode::Unmodified {
                worktree_status
            } else {
                index_status
            }),
        };

        proto::StatusEntry {
            repo_path: self.repo_path.as_unix_str().to_owned(),
            simple_status,
            status: Some(status_to_proto(self.status)),
            diff_stat_added: self.diff_stat.map(|ds| ds.added),
            diff_stat_deleted: self.diff_stat.map(|ds| ds.deleted),
            staged_diff_stat_added: self.staged_diff_stat.map(|ds| ds.added),
            staged_diff_stat_deleted: self.staged_diff_stat.map(|ds| ds.deleted),
            unstaged_diff_stat_added: self.unstaged_diff_stat.map(|ds| ds.added),
            unstaged_diff_stat_deleted: self.unstaged_diff_stat.map(|ds| ds.deleted),
        }
    }
}

impl TryFrom<proto::StatusEntry> for StatusEntry {
    type Error = anyhow::Error;

    fn try_from(value: proto::StatusEntry) -> Result<Self, Self::Error> {
        let repo_path = RepoPath::from_proto(&value.repo_path).context("invalid repo path")?;
        let status = status_from_proto(value.simple_status, value.status)?;
        let diff_stat = match (value.diff_stat_added, value.diff_stat_deleted) {
            (Some(added), Some(deleted)) => Some(DiffStat { added, deleted }),
            _ => None,
        };
        let staged_diff_stat = match (value.staged_diff_stat_added, value.staged_diff_stat_deleted)
        {
            (Some(added), Some(deleted)) => Some(DiffStat { added, deleted }),
            _ => None,
        };
        let unstaged_diff_stat = match (
            value.unstaged_diff_stat_added,
            value.unstaged_diff_stat_deleted,
        ) {
            (Some(added), Some(deleted)) => Some(DiffStat { added, deleted }),
            _ => None,
        };
        Ok(Self {
            repo_path,
            status,
            diff_stat,
            staged_diff_stat,
            unstaged_diff_stat,
        })
    }
}

impl sum_tree::Item for StatusEntry {
    type Summary = PathSummary<GitSummary>;

    fn summary(&self, _: <Self::Summary as sum_tree::Summary>::Context<'_>) -> Self::Summary {
        PathSummary {
            max_path: self.repo_path.as_ref().clone(),
            item_summary: self.status.summary(),
        }
    }
}

impl sum_tree::KeyedItem for StatusEntry {
    type Key = PathKey;

    fn key(&self) -> Self::Key {
        PathKey(self.repo_path.as_ref().clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepositoryId(pub u64);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeDetails {
    pub merge_heads_by_conflicted_path: TreeMap<RepoPath, Vec<Option<SharedString>>>,
    pub message: Option<SharedString>,
}

/// What a push did, which is more than the remote's own output whenever it did not do quite what
/// was asked.
#[derive(Clone, Debug)]
pub struct PushOutput {
    pub output: RemoteCommandOutput,
    /// The push was asked for without a lease, but it ran as `--force-with-lease` because the
    /// peer's server predates [`push_options_from_proto`] and never saw the request.
    pub kept_lease_against_request: bool,
}

impl PushOutput {
    /// A push that did what it was asked, which is every local one.
    pub fn honoured(output: RemoteCommandOutput) -> Self {
        Self {
            output,
            kept_lease_against_request: false,
        }
    }
}

/// What the merge-request box asks [`Repository::build_merge_request_branch`] to build.
#[derive(Clone, Debug)]
pub struct MergeRequestBuild {
    /// The ref the merge request starts from, as the user picked it. It is resolved to a commit
    /// once, before anything moves.
    pub base: String,
    pub shas_oldest_first: Vec<String>,
    pub merge_request_branch: String,
    pub message: String,
    /// Whether an existing `merge_request_branch` may be moved to the new commit.
    pub recreate: bool,
    /// The branch checked out now, which the caller has already remembered as the way back from
    /// `scratch_branch`.
    pub origin_branch: String,
    pub scratch_branch: String,
}

/// The rest of a merge-request build that stopped on a conflict, once the cherry-pick has been
/// continued: see [`Repository::finish_merge_request_branch`].
#[derive(Clone, Debug)]
pub struct MergeRequestContinuation {
    /// The commit the build resolved its base to when it started.
    pub base_sha: String,
    pub merge_request_branch: String,
    pub message: String,
    pub recreate: bool,
    pub origin_branch: String,
    pub scratch_branch: String,
}

/// Where a merge-request build, or its continuation, ended up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeRequestBuildOutcome {
    /// The merge-request branch points at the new commit.
    Created {
        merge_request_sha: String,
        /// Where a recreated branch pointed before, which is how the user finds it again.
        previous_sha: Option<String>,
        /// Why the user could not be put back on the branch they started from, when that is the
        /// one step that went wrong. They are still on the scratch branch then.
        return_failure: Option<String>,
    },
    /// A cherry-pick stopped on a conflict, leaving the user on the scratch branch.
    Stopped { base_sha: String, reason: String },
    /// git refused a step, or there was nothing to build.
    Refused {
        reason: String,
        /// Whether the user is still on the scratch branch. A fresh build puts everything back
        /// whenever it can; a continuation never throws the resolved commits away.
        left_on_scratch_branch: bool,
    },
}

#[derive(Clone)]
pub enum CommitDataState {
    Loading(Option<Shared<oneshot::Receiver<Arc<CommitData>>>>),
    Loaded(Arc<CommitData>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub id: RepositoryId,
    pub statuses_by_path: SumTree<StatusEntry>,
    pub work_directory_abs_path: Arc<Path>,
    pub dot_git_abs_path: Arc<Path>,
    /// Absolute path to the directory holding this worktree's Git state.
    ///
    /// For a linked worktree this is the worktree-specific directory under the
    /// common Git directory, such as `<main>/.git/worktrees/<name>`.
    pub repository_dir_abs_path: Arc<Path>,
    /// Absolute path to the repository's common Git directory.
    ///
    /// For a normal checkout this is `<work_directory>/.git`. For a linked
    /// worktree this is the common Git directory shared by all worktrees. If
    /// that common directory is a bare repository, there may be no main
    /// worktree path to derive from it.
    pub common_dir_abs_path: Arc<Path>,
    pub path_style: PathStyle,
    pub branch: Option<Branch>,
    pub branch_list: Arc<[Branch]>,
    pub branch_list_error: Option<SharedString>,
    /// Why the file statuses could not be read: a failed status refresh, or a
    /// repository that could not be opened. While set, `statuses_by_path` may
    /// be empty or stale. Only the next successful full status scan clears it.
    pub status_error: Option<SharedString>,
    pub head_commit: Option<CommitDetails>,
    pub scan_id: u64,
    pub merge: MergeDetails,
    pub remote_origin_url: Option<String>,
    pub remote_upstream_url: Option<String>,
    pub stash_entries: GitStash,
    pub linked_worktrees: Arc<[GitWorktree]>,
    /// How many refs the repository keeps as loose files. `None` when that is not known (a host
    /// too old to say, or refs that could not be read), in which case nothing should suggest
    /// packing them.
    pub loose_refs: Option<LooseRefCount>,
    /// [`RefStorageScan::tags_fingerprint`] from the host: it changes whenever the tags may have
    /// changed, which [`RepositoryEvent::TagsChanged`] announces. `None` when the host cannot
    /// tell; views that list tags then have to reload them whenever the branches or HEAD change
    /// instead.
    pub tags_fingerprint: Option<u64>,
    /// ADR-F90: the repository was there when the user opened its folder, or the user made it in
    /// Zed. See [`GitStore::repository_opened_by_user`].
    pub opened_by_user: bool,
}

type JobId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobInfo {
    pub start: Instant,
    pub message: SharedString,
}

struct CommitDataHandler {
    _task: Task<()>,
    commit_data_request: async_channel::Sender<Oid>,
    completion_senders: HashMap<Oid, oneshot::Sender<Arc<CommitData>>>,
    pending_requests: HashSet<Oid>,
}

/// Represents the handler of a git cat-file --batch process within Zed
/// It's used to lazily fetch commit data as needed (whatever a user is viewing)
enum CommitDataHandlerState {
    /// The handler is open and processing requests
    Open(CommitDataHandler),
    /// The handler closed because it didn't receive any requests in the last 10s
    /// or hasn't been open before
    Closed,
}

enum NextCommitDataRequest {
    Request(BoxFuture<'static, Result<proto::GetCommitDataResponse>>),
    Idle,
    Closed,
}

pub struct InitialGitGraphData {
    fetch_task: Task<()>,
    pub error: Option<SharedString>,
    pub commit_data: Vec<Arc<InitialGraphCommitData>>,
    pub commit_oid_to_index: HashMap<Oid, usize>,
    subscribers: Vec<async_channel::Sender<Result<Vec<Arc<InitialGraphCommitData>>, SharedString>>>,
}

pub struct GraphDataResponse<'a> {
    pub commits: &'a [Arc<InitialGraphCommitData>],
    pub is_loading: bool,
    pub error: Option<SharedString>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum UnshallowState {
    #[default]
    Idle,
    InProgress,
    Unshallowed,
}

pub struct Repository {
    this: WeakEntity<Self>,
    snapshot: RepositorySnapshot,
    unshallow_state: UnshallowState,
    commit_message_buffer: Option<Entity<Buffer>>,
    git_store: WeakEntity<GitStore>,
    blob_read_limiter: Arc<Semaphore>,
    // For a local repository, holds paths that have had worktree events since the last status scan completed,
    // and that should be examined during the next status scan.
    paths_needing_status_update: Vec<Vec<RepoPath>>,
    // Scans queued under `GitJobKey::ReloadGitState` coalesce, and it is the
    // *last* one that survives. Accumulating what every queued scan asked for
    // here means the surviving job still covers the requests that were dropped.
    pending_snapshot_scan: SnapshotScan,
    job_sender: mpsc::UnboundedSender<GitJob>,
    _worker_task: Task<()>,
    active_jobs: HashMap<JobId, JobInfo>,
    job_debug_queue: job_debug_queue::GitJobDebugQueue,
    pending_ops: SumTree<PendingOps>,
    job_id: JobId,
    askpass_delegates: RemoteAskPassDelegates,
    latest_askpass_id: u64,
    repository_state: Shared<Task<Result<RepositoryState, String>>>,
    initial_graph_data: HashMap<(LogSource, LogOrder), InitialGitGraphData>,
    commit_data_handler: CommitDataHandlerState,
    commit_data: HashMap<Oid, CommitDataState>,
    /// ADR-F93: on a remote server, the account the client picked for this repository.
    identity_from_client: Option<Arc<git::account::GitIdentity>>,
    /// ADR-F93: on a client of a remote server, what the host was told of the picked account.
    pub(crate) remote_git_account: crate::git_accounts::RemoteGitAccount,
}

/// Which halves of a repository's state a scheduled scan recomputes.
///
/// Splitting the scan lets a `.git` event that only touched refs skip the
/// expensive `git status` / `git diff --numstat` pass, and an event that only
/// touched the index skip the ref enumeration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SnapshotScan {
    /// Branch list, head commit, linked worktrees and remote URLs.
    refs: bool,
    /// File statuses, diff stats, stash entries and merge conflicts.
    statuses: bool,
}

impl SnapshotScan {
    const NONE: Self = Self {
        refs: false,
        statuses: false,
    };
    const FULL: Self = Self {
        refs: true,
        statuses: true,
    };

    /// Anything that cannot be attributed to a specific part of the git
    /// directory has to be treated as if everything changed.
    fn for_git_dir_change(change: Option<GitDirChange>) -> Self {
        match change {
            Some(GitDirChange::Refs) => Self {
                refs: true,
                statuses: false,
            },
            Some(GitDirChange::Index) => Self {
                refs: false,
                statuses: true,
            },
            Some(GitDirChange::Unknown) | None => Self::FULL,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            refs: self.refs || other.refs,
            statuses: self.statuses || other.statuses,
        }
    }
}

type RemoteAskPassDelegates = Arc<Mutex<HashMap<u64, RemoteAskPassDelegate>>>;

struct RemoteAskPassDelegate {
    delegate: AskPassDelegate,
    active_request_cancellation: Option<oneshot::Sender<()>>,
}

struct RemoteAskPassOperation {
    askpass_id: u64,
    delegates: RemoteAskPassDelegates,
}

impl RemoteAskPassOperation {
    fn new(askpass_id: u64, delegate: AskPassDelegate, delegates: RemoteAskPassDelegates) -> Self {
        let previous = delegates.lock().insert(
            askpass_id,
            RemoteAskPassDelegate {
                delegate,
                active_request_cancellation: None,
            },
        );
        debug_assert!(previous.is_none());
        Self {
            askpass_id,
            delegates,
        }
    }
}

impl Drop for RemoteAskPassOperation {
    fn drop(&mut self) {
        let delegate = self.delegates.lock().remove(&self.askpass_id);
        debug_assert!(delegate.is_some());
    }
}

impl std::ops::Deref for Repository {
    type Target = RepositorySnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

#[derive(Clone)]
pub struct LocalRepositoryState {
    pub fs: Arc<dyn Fs>,
    pub backend: Arc<dyn GitRepository>,
    pub environment: Arc<HashMap<String, String>>,
}

impl LocalRepositoryState {
    async fn new(
        work_directory_abs_path: Arc<Path>,
        dot_git_abs_path: Arc<Path>,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        is_trusted: bool,
        cx: &mut AsyncApp,
    ) -> anyhow::Result<Self> {
        let environment = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.local_directory_environment(&Shell::System, work_directory_abs_path.clone(), cx)
                })?
                .await
                .unwrap_or_else(|| {
                    log::error!("failed to get working directory environment for repository {work_directory_abs_path:?}");
                    HashMap::default()
                });
        let search_paths = environment.get("PATH").map(|val| val.to_owned());
        let backend = cx
            .background_spawn({
                let fs = fs.clone();
                async move {
                    let system_git_binary_path = search_paths
                        .and_then(|search_paths| {
                            which::which_in("git", Some(search_paths), &work_directory_abs_path)
                                .ok()
                        })
                        .or_else(|| which::which("git").ok());
                    fs.open_repo(&dot_git_abs_path, system_git_binary_path.as_deref())
                        .with_context(|| format!("opening repository at {dot_git_abs_path:?}"))
                }
            })
            .await?;
        backend.set_trusted(is_trusted);
        Ok(LocalRepositoryState {
            backend,
            environment: Arc::new(environment),
            fs,
        })
    }
}

#[derive(Clone)]
pub struct RemoteRepositoryState {
    pub project_id: ProjectId,
    pub client: AnyProtoClient,
}

#[derive(Clone)]
pub enum RepositoryState {
    Local(LocalRepositoryState),
    Remote(RemoteRepositoryState),
}

enum PermalinkTarget {
    Buffer {
        buffer_id: BufferId,
        selection: Range<u32>,
    },
    File(ProjectPath),
}

impl PermalinkTarget {
    fn selection(&self) -> Option<Range<u32>> {
        match self {
            Self::Buffer { selection, .. } => Some(selection.clone()),
            Self::File(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitGraphEvent {
    CountUpdated(usize),
    FullyLoaded,
    LoadingError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepositoryEvent {
    StatusesChanged,
    HeadChanged,
    BranchListChanged,
    /// A tag may have been added, moved or deleted, including by a `git tag` or `git fetch
    /// --tags` run outside Zed. Never sent while `RepositorySnapshot::tags_fingerprint` is
    /// `None`.
    TagsChanged,
    StashEntriesChanged,
    GitWorktreeListChanged,
    PendingOpsChanged {
        pending_ops: SumTree<PendingOps>,
    },
    GraphEvent((LogSource, LogOrder), GitGraphEvent),
}

#[derive(Clone, Debug)]
pub struct JobsUpdated;

#[derive(Debug)]
pub enum GitStoreEvent {
    ActiveRepositoryChanged(Option<RepositoryId>),
    /// Bool is true when the repository that's updated is the active repository
    RepositoryUpdated(RepositoryId, RepositoryEvent, bool),
    RepositoryAdded,
    RepositoryRemoved(RepositoryId),
    IndexWriteError(anyhow::Error),
    JobsUpdated,
    ConflictsUpdated,
    GlobalConfigurationUpdated,
    DiffBaseChanged(Option<RepositoryId>),
}

impl EventEmitter<RepositoryEvent> for Repository {}
impl EventEmitter<JobsUpdated> for Repository {}
impl EventEmitter<GitStoreEvent> for GitStore {}

pub struct GitJob {
    id: JobId,
    job: Box<dyn FnOnce(RepositoryState, &mut AsyncApp) -> Task<()>>,
    key: Option<GitJobKey>,
}

#[derive(PartialEq, Eq)]
enum GitJobKey {
    WriteIndex(Vec<RepoPath>),
    ReloadBufferDiffBases,
    RefreshStatuses,
    ReloadGitState,
    SequencerState,
    /// A view's tag list: only its newest request is worth running.
    LatestTags(EntityId),
    /// A view's `git describe` of the commit it has selected: only the newest is worth running.
    LatestDescribe(EntityId),
}

impl GitStore {
    pub fn local(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _fs_watches = if fs.is_fake() {
            Box::new([])
        } else {
            [
                config_dir().join("git/config"),
                home_dir().join(".gitconfig"),
            ]
            .into_iter()
            .map(|path| {
                let fs = fs.clone();

                cx.spawn(async move |this, cx| {
                    let watcher = fs.watch(&path, Duration::from_millis(100));
                    let (mut watcher, _) = watcher.await;
                    while let Some(_) = watcher.next().await {
                        let Ok(_) = this.update(cx, |this, cx| {
                            let GitStoreState::Local {
                                project_environment,
                                fs,
                                ..
                            } = &this.state
                            else {
                                return;
                            };
                            let project_environment = project_environment.downgrade();
                            let fs = fs.clone();
                            let repositories_to_respawn = this
                                .repositories
                                .iter()
                                .filter_map(|(repository_id, repo)| {
                                    repo.read(cx)
                                        .job_sender
                                        .is_closed()
                                        .then_some((*repository_id, repo.clone()))
                                })
                                .collect::<Vec<_>>();
                            for (repository_id, repo) in repositories_to_respawn {
                                let is_trusted = this.repository_is_trusted(repository_id, cx);
                                repo.update(cx, |repo, cx| {
                                    repo.respawn_local_worker(
                                        project_environment.clone(),
                                        fs.clone(),
                                        is_trusted,
                                        cx,
                                    );
                                    repo.schedule_scan(None, cx);
                                })
                            }
                            let display_repo_ids =
                                this.display_diffs.keys().copied().collect::<Vec<_>>();
                            for repository_id in display_repo_ids {
                                this.refresh_diff_base_for_repo(repository_id, cx);
                            }
                            cx.emit(GitStoreEvent::GlobalConfigurationUpdated);
                        }) else {
                            return;
                        };
                    }
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
        };

        Self::new(
            worktree_store.clone(),
            buffer_store,
            GitStoreState::Local {
                next_repository_id: Arc::new(AtomicU64::new(1)),
                downstream: None,
                project_environment: environment,
                _fs_watches,
                fs,
            },
            cx,
        )
    }

    pub fn remote(
        worktree_store: &Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        upstream_client: AnyProtoClient,
        project_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(
            worktree_store.clone(),
            buffer_store,
            GitStoreState::Remote {
                upstream_client,
                upstream_project_id: project_id,
                downstream: None,
            },
            cx,
        )
    }

    fn new(
        worktree_store: Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
        state: GitStoreState,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut _subscriptions = vec![
            cx.subscribe(&worktree_store, Self::on_worktree_store_event),
            cx.subscribe(&buffer_store, Self::on_buffer_store_event),
        ];

        if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
            _subscriptions.push(cx.subscribe(&trusted_worktrees, Self::on_trusted_worktrees_event));
        }

        _subscriptions.push(cx.observe_global::<SettingsStore>(|this, cx| {
            let setting = ProjectSettings::get_global(cx).git.diff_base;
            if setting != this.diff_base {
                this.set_diff_base(setting, cx);
            }
            this.activate_parked_repositories_where_parking_disabled(cx);
            this.apply_git_accounts(cx);
        }));

        let diff_base_setting = ProjectSettings::get_global(cx).git.diff_base;
        let existing_worktrees = worktree_store.read(cx).worktrees().collect::<Vec<_>>();
        let mut this = GitStore {
            state,
            project: None,
            buffer_store,
            worktree_store,
            repositories: HashMap::default(),
            parked_repositories: Vec::new(),
            diff_base: diff_base_setting,
            display_diffs: HashMap::default(),
            worktree_ids: HashMap::default(),
            active_repo_id: None,
            _subscriptions,
            loading_diffs: HashMap::default(),
            shared_diffs: HashMap::default(),
            commit_searches: HashMap::default(),
            blob_read_limiter: Arc::new(Semaphore::new(MAX_CONCURRENT_BLOB_READS)),
            worktrees_in_initial_scan: HashSet::default(),
            repositories_opened_by_user: Rc::default(),
            remote_git_account_connection: None,
            diffs: HashMap::default(),
            buffer_ids_by_index_text_buffer_id: HashMap::default(),
        };
        for worktree in &existing_worktrees {
            this.track_initial_scan(worktree, cx);
        }
        this
    }

    /// Notes that `worktree`'s first scan is under way until it finishes (ADR-F90).
    fn track_initial_scan(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        if !matches!(self.state, GitStoreState::Local { .. }) {
            return;
        }
        let Some(scan_complete) = worktree
            .read(cx)
            .as_local()
            .map(|worktree| worktree.scan_complete())
        else {
            return;
        };
        let worktree_id = worktree.read(cx).id();
        self.worktrees_in_initial_scan.insert(worktree_id);
        cx.spawn(async move |this, cx| {
            scan_complete.await;
            this.update(cx, |this, _| {
                this.worktrees_in_initial_scan.remove(&worktree_id);
            })
            .ok();
        })
        .detach();
    }

    /// Whether `repository` is one the user opened, in which an AI agent's `git` tool may run
    /// (ADR-F90): found when its folder was opened, or made with [`Self::git_init`]. On a remote
    /// project the host decides, and a host that does not say counts as no.
    pub fn repository_opened_by_user(&self, repository: &Entity<Repository>, cx: &App) -> bool {
        let repository = repository.read(cx);
        repository.snapshot.opened_by_user
            || (matches!(self.state, GitStoreState::Local { .. })
                && self
                    .repositories_opened_by_user
                    .borrow()
                    .contains(&repository.work_directory_abs_path))
    }

    pub(crate) fn set_project(&mut self, project: WeakEntity<Project>) {
        self.project = Some(project);
    }

    /// ForgeKairos (ADR-F93): whether the git accounts in the settings pick who this store's
    /// repositories commit as. Only where the repositories are the user's own: a remote server is
    /// sent the user's settings too, whose `project_accounts` are keyed by the client's paths.
    pub fn applies_git_accounts(&self) -> bool {
        match &self.state {
            GitStoreState::Local { downstream, .. } => {
                !downstream.as_ref().is_some_and(|downstream| {
                    downstream.project_id.0 == proto::REMOTE_SERVER_PROJECT_ID
                })
            }
            GitStoreState::Remote { .. } => false,
        }
    }

    fn apply_git_accounts(&mut self, cx: &mut Context<Self>) {
        let applies = self.applies_git_accounts();
        let remote_connection = self.remote_git_account_connection().map(str::to_string);
        for repository in self.repositories.values() {
            repository.update(cx, |repository, cx| {
                if let Some(remote_connection) = &remote_connection {
                    repository.send_remote_git_account(remote_connection, cx);
                } else {
                    repository.apply_git_account(applies, cx).detach();
                }
            });
        }
    }

    /// ADR-F93: makes this store, a client of a remote server, pick its repositories' git
    /// accounts under `connection` (the host's `RemoteConnectionIdentity::persistence_key`)
    /// and tell the host.
    pub fn set_remote_git_account_connection(
        &mut self,
        connection: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.remote_git_account_connection = connection;
        self.apply_git_accounts(cx);
    }

    /// The host connection the choices of this store's repositories are kept under: only for a
    /// client of a remote server (not a collaborator's guest, whose host has its own accounts).
    pub fn remote_git_account_connection(&self) -> Option<&str> {
        match &self.state {
            GitStoreState::Remote {
                upstream_project_id,
                ..
            } if *upstream_project_id == proto::REMOTE_SERVER_PROJECT_ID => {
                self.remote_git_account_connection.as_deref()
            }
            _ => None,
        }
    }

    /// The key `repository`'s git account is picked under in `git.project_accounts`, or `None`
    /// where git accounts do not apply.
    pub fn git_account_key(&self, repository: &Repository) -> Option<String> {
        if self.applies_git_accounts() {
            Some(crate::git_accounts::project_account_key(
                &repository.work_directory_abs_path,
            ))
        } else {
            self.remote_git_account_connection().map(|connection| {
                crate::git_accounts::remote_project_account_key(
                    connection,
                    &repository.work_directory_abs_path,
                )
            })
        }
    }

    /// Whether the user trusts a worktree of `worktree_store` that `work_directory_abs_path`, a
    /// repository on the remote host, is in or holds. A local repository's backend knows its own
    /// trust.
    fn remote_repository_is_trusted(
        worktree_store: &Entity<WorktreeStore>,
        work_directory_abs_path: &Path,
        cx: &mut App,
    ) -> bool {
        let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
            return false;
        };
        let worktree_ids = worktree_store
            .read(cx)
            .worktrees()
            .filter(|worktree| {
                let worktree_path = worktree.read(cx).abs_path();
                work_directory_abs_path.starts_with(worktree_path.as_ref())
                    || worktree_path.starts_with(work_directory_abs_path)
            })
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>();
        worktree_ids.into_iter().any(|worktree_id| {
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.can_trust(worktree_store, worktree_id, cx)
            })
        })
    }

    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_get_remotes);
        client.add_entity_request_handler(Self::handle_get_branches);
        client.add_entity_request_handler(Self::handle_get_default_branch);
        client.add_entity_request_handler(Self::handle_change_branch);
        client.add_entity_request_handler(Self::handle_create_branch);
        client.add_entity_request_handler(Self::handle_rename_branch);
        client.add_entity_request_handler(Self::handle_create_remote);
        client.add_entity_request_handler(Self::handle_remove_remote);
        client.add_entity_request_handler(Self::handle_run_command);
        client.add_entity_request_handler(Self::handle_delete_branch);
        client.add_entity_request_handler(Self::handle_git_init);
        client.add_entity_request_handler(Self::handle_push);
        client.add_entity_request_handler(Self::handle_pull);
        client.add_entity_request_handler(Self::handle_fetch);
        client.add_entity_request_handler(Self::handle_stage);
        client.add_entity_request_handler(Self::handle_unstage);
        client.add_entity_request_handler(Self::handle_stash);
        client.add_entity_request_handler(Self::handle_stash_pop);
        client.add_entity_request_handler(Self::handle_stash_apply);
        client.add_entity_request_handler(Self::handle_stash_drop);
        client.add_entity_request_handler(Self::handle_commit);
        client.add_entity_request_handler(Self::handle_set_git_account_identity);
        client.add_entity_request_handler(Self::handle_run_hook);
        client.add_entity_request_handler(Self::handle_reset);
        client.add_entity_request_handler(Self::handle_show);
        client.add_entity_request_handler(Self::handle_create_checkpoint);
        client.add_entity_request_handler(Self::handle_create_archive_checkpoint);
        client.add_entity_request_handler(Self::handle_restore_checkpoint);
        client.add_entity_request_handler(Self::handle_restore_archive_checkpoint);
        client.add_entity_request_handler(Self::handle_compare_checkpoints);
        client.add_entity_request_handler(Self::handle_diff_checkpoints);
        client.add_entity_request_handler(Self::handle_load_commit_diff);
        client.add_entity_request_handler(Self::handle_checkout_files);
        client.add_entity_request_handler(Self::handle_add_path_to_gitignore);
        client.add_entity_request_handler(Self::handle_add_path_to_git_info_exclude);
        client.add_entity_request_handler(Self::handle_open_commit_message_buffer);
        client.add_entity_request_handler(Self::handle_set_index_text);
        client.add_entity_request_handler(Self::handle_askpass);
        client.add_entity_request_handler(Self::handle_check_for_pushed_commits);
        client.add_entity_request_handler(Self::handle_git_diff);
        client.add_entity_request_handler(Self::handle_tree_diff);
        client.add_entity_request_handler(Self::handle_get_blob_content);
        client.add_entity_request_handler(Self::handle_load_commit_template);
        client.add_entity_request_handler(Self::handle_open_unstaged_diff);
        client.add_entity_request_handler(Self::handle_open_uncommitted_diff);
        client.add_entity_message_handler(Self::handle_update_diff_bases);
        client.add_entity_request_handler(Self::handle_get_file_permalink);
        client.add_entity_request_handler(Self::handle_get_permalink_to_line);
        client.add_entity_request_handler(Self::handle_blame_buffer);
        client.add_entity_request_handler(Self::handle_blame_buffer_at_revision);
        client.add_entity_message_handler(Self::handle_update_repository);
        client.add_entity_message_handler(Self::handle_remove_repository);
        client.add_entity_request_handler(Self::handle_git_clone);
        client.add_entity_request_handler(Self::handle_get_worktrees);
        client.add_entity_request_handler(Self::handle_create_worktree);
        client.add_entity_request_handler(Self::handle_remove_worktree);
        client.add_entity_request_handler(Self::handle_rename_worktree);
        client.add_entity_request_handler(Self::handle_worktree_created_at);
        client.add_entity_request_handler(Self::handle_get_head_sha);
        client.add_entity_request_handler(Self::handle_edit_ref);
        client.add_entity_request_handler(Self::handle_repair_worktrees);
        client.add_entity_request_handler(Self::handle_get_commit_data);
        client.add_entity_stream_request_handler(Self::handle_get_initial_graph_data);
        client.add_entity_stream_request_handler(Self::handle_search_commits);
        client.add_entity_message_handler(Self::handle_cancel_search_commits);
    }

    pub fn is_local(&self) -> bool {
        matches!(self.state, GitStoreState::Local { .. })
    }

    fn set_active_repo_id(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if self.active_repo_id != Some(repo_id) {
            self.active_repo_id = Some(repo_id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(repo_id)));
        }
    }

    pub fn set_active_repo_for_path(&mut self, project_path: &ProjectPath, cx: &mut Context<Self>) {
        if let Some((repo, _)) = self.repository_and_path_for_project_path(project_path, cx) {
            self.set_active_repo_id(repo.read(cx).id, cx);
        }
    }

    pub fn set_active_repo_for_worktree(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return;
        };
        let worktree_abs_path = worktree.read(cx).abs_path();
        let Some(repo_id) = self
            .repositories
            .values()
            .filter(|repo| {
                let repo_path = &repo.read(cx).work_directory_abs_path;
                // The folder opened in Zed isn't necessarily the repo root; it may be
                // a subdirectory of it, e.g. opening `~/code/myrepo/backend` when the
                // repo lives at `~/code/myrepo`. So match any repo whose work directory
                // contains the folder. Nested repos can produce multiple matches, e.g.
                // opening `~/code/myrepo/vendor/lib` where `vendor/lib` is a submodule
                // matches both `myrepo` and the submodule; `max_by_key` then picks the
                // innermost match (the submodule), which the folder actually belongs to.
                worktree_abs_path.starts_with(repo_path.as_ref())
            })
            .max_by_key(|repo| repo.read(cx).work_directory_abs_path.as_os_str().len())
            .map(|repo| repo.read(cx).id)
        else {
            return;
        };

        self.set_active_repo_id(repo_id, cx);
    }

    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) {
        match &mut self.state {
            GitStoreState::Remote {
                downstream: downstream_client,
                ..
            } => {
                for repo in self.repositories.values() {
                    let update = repo.read(cx).snapshot.initial_update(project_id);
                    for update in split_repository_update(update) {
                        client.send(update).log_err();
                    }
                }
                *downstream_client = Some((client, ProjectId(project_id)));
            }
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => {
                let mut snapshots = HashMap::default();
                let (updates_tx, mut updates_rx) = mpsc::unbounded();
                for repo in self.repositories.values() {
                    updates_tx
                        .unbounded_send(DownstreamUpdate::UpdateRepository(
                            repo.read(cx).snapshot.clone(),
                        ))
                        .ok();
                }
                *downstream_client = Some(LocalDownstreamState {
                    client: client.clone(),
                    project_id: ProjectId(project_id),
                    updates_tx,
                    _task: cx.spawn(async move |this, cx| {
                        cx.background_spawn(async move {
                            while let Some(update) = updates_rx.next().await {
                                match update {
                                    DownstreamUpdate::UpdateRepository(snapshot) => {
                                        if let Some(old_snapshot) = snapshots.get_mut(&snapshot.id)
                                        {
                                            let update =
                                                snapshot.build_update(old_snapshot, project_id);
                                            *old_snapshot = snapshot;
                                            for update in split_repository_update(update) {
                                                client.send(update)?;
                                            }
                                        } else {
                                            let update = snapshot.initial_update(project_id);
                                            for update in split_repository_update(update) {
                                                client.send(update)?;
                                            }
                                            snapshots.insert(snapshot.id, snapshot);
                                        }
                                    }
                                    DownstreamUpdate::RemoveRepository(id) => {
                                        client.send(proto::RemoveRepository {
                                            project_id,
                                            id: id.to_proto(),
                                        })?;
                                    }
                                }
                            }
                            anyhow::Ok(())
                        })
                        .await
                        .ok();
                        this.update(cx, |this, _| {
                            if let GitStoreState::Local {
                                downstream: downstream_client,
                                ..
                            } = &mut this.state
                            {
                                downstream_client.take();
                            } else {
                                unreachable!("unshared called on remote store");
                            }
                        })
                    }),
                });
                // ADR-F93: shared to a client as its remote server, the accounts are the client's.
                self.apply_git_accounts(cx);
            }
        }
    }

    pub fn unshared(&mut self, _cx: &mut Context<Self>) {
        match &mut self.state {
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => {
                downstream_client.take();
            }
            GitStoreState::Remote {
                downstream: downstream_client,
                ..
            } => {
                downstream_client.take();
            }
        }
        self.shared_diffs.clear();
    }

    pub(crate) fn forget_shared_diffs_for(&mut self, peer_id: &proto::PeerId) {
        self.shared_diffs.remove(peer_id);
    }

    pub fn active_repository(&self) -> Option<Entity<Repository>> {
        self.active_repo_id
            .as_ref()
            .map(|id| self.repositories[id].clone())
    }

    fn file_is_symlink(file: &File, cx: &App) -> bool {
        file.worktree
            .read(cx)
            .entry_for_path(&file.path)
            .is_some_and(|entry| entry.canonical_path.is_some())
    }

    fn buffer_is_symlink(buffer: &Entity<Buffer>, cx: &App) -> bool {
        File::from_dyn(buffer.read(cx).file()).is_some_and(|file| Self::file_is_symlink(file, cx))
    }

    pub fn open_unstaged_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();
        if let Some(diff_state) = self.diffs.get(&buffer_id)
            && let Some(unstaged_diff) = diff_state
                .read(cx)
                .unstaged_diff
                .as_ref()
                .and_then(|weak| weak.upgrade())
        {
            // If this unstaged diff was first opened as the uncommitted diff's
            // secondary, its index text wasn't highlighted. Enable it now and
            // recalc so the language gets applied to the deleted (base) side.
            diff_state.update(cx, |diff_state, cx| {
                if !diff_state.index_text_buffer_language_enabled {
                    diff_state.index_text_buffer_language_enabled = true;
                    let buffer_snapshot = buffer.read(cx).text_snapshot();
                    diff_state.recalculate_diffs(buffer_snapshot, cx);
                }
            });
            if let Some(task) =
                diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
            {
                return cx.background_executor().spawn(async move {
                    task.await;
                    Ok(unstaged_diff)
                });
            }
            return Task::ready(Ok(unstaged_diff));
        }

        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find git repository for buffer")));
        };

        let is_symlink = Self::buffer_is_symlink(&buffer, cx);
        let task = self
            .loading_diffs
            .entry((buffer_id, DiffKind::Unstaged))
            .or_insert_with(|| {
                let staged_text = if is_symlink {
                    Task::ready(Ok(None))
                } else {
                    repo.update(cx, |repo, cx| {
                        repo.load_staged_text(buffer_id, repo_path, cx)
                    })
                };
                cx.spawn(async move |this, cx| {
                    Self::open_diff_internal(
                        this,
                        DiffKind::Unstaged,
                        staged_text.await.map(DiffBasesChange::SetIndex),
                        buffer,
                        cx,
                    )
                    .await
                    .map_err(Arc::new)
                })
                .shared()
            })
            .clone();

        cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    /// Opens the staged (HEAD-vs-index) diff for the given buffer, along with
    /// the index text buffer that is the diff's main buffer.
    pub fn open_staged_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(Entity<BufferDiff>, Entity<Buffer>)>> {
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(diff_state) = self.diffs.get(&buffer_id)
            && let Some(staged_diff) = diff_state.read(cx).staged_diff_and_index_text_buffer()
        {
            if let Some(task) =
                diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
            {
                return cx.background_executor().spawn(async move {
                    task.await;
                    Ok(staged_diff)
                });
            }
            return Task::ready(Ok(staged_diff));
        }

        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find git repository for buffer")));
        };

        let task = self
            .loading_diffs
            .entry((buffer_id, DiffKind::Staged))
            .or_insert_with(|| {
                let changes = repo.update(cx, |repo, cx| {
                    repo.load_committed_text(buffer_id, repo_path, cx)
                });

                cx.spawn(async move |this, cx| {
                    Self::open_diff_internal(this, DiffKind::Staged, changes.await, buffer, cx)
                        .await
                        .map_err(Arc::new)
                })
                .shared()
            })
            .clone();

        cx.spawn(async move |this, cx| {
            let diff = task.await.map_err(|e| anyhow!("{e}"))?;
            this.update(cx, |this, cx| {
                let index_text_buffer = this
                    .diffs
                    .get(&buffer_id)
                    .and_then(|diff_state| {
                        let (_, index_text_buffer) = diff_state.read(cx).staged_diff.as_ref()?;
                        Some(index_text_buffer.clone())
                    })
                    .context("index text buffer missing after opening staged diff")?;
                Ok((diff, index_text_buffer))
            })?
        })
    }

    /// Stages the worktree changes covered by `worktree_ranges`, acting on the
    /// given unstaged (index-vs-worktree) diff. Used by both the unstaged-changes
    /// view and the uncommitted (gutter) controls: "stage" means the same index
    /// change regardless of which view it was invoked from, so callers holding an
    /// uncommitted diff pass its unstaged secondary.
    ///
    /// Decomposes the worktree region into the unstaged hunks it covers, so no
    /// worktree->index projection is needed. Optimistically suppresses the staged
    /// hunks from the unstaged diff and, if the uncommitted diff happens to be
    /// open, marks the corresponding uncommitted hunks as staging.
    pub fn stage_hunks(
        &mut self,
        buffer: Entity<Buffer>,
        unstaged_diff: Entity<BufferDiff>,
        worktree_ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if worktree_ranges.is_empty() {
            return Ok(());
        }
        let buffer_snapshot = buffer.read(cx).snapshot();
        let buffer_id = buffer_snapshot.remote_id();
        let file_exists = buffer_snapshot
            .file()
            .is_some_and(|file| file.disk_state().exists());

        let unstaged_snapshot = unstaged_diff.read(cx).snapshot(cx);

        // Decompose: the unstaged hunks the worktree region covers carry the
        // index range directly. Sorting by buffer offset also sorts by index
        // offset, since the hunks are non-overlapping. We read the raw hunks
        // (ignoring optimistic suppression) so that re-staging a hunk that was
        // optimistically staged and then unstaged still finds it. The footprints
        // let the patch drop earlier edits the region covers even where the raw
        // diff is empty (re-staging a hunk that is staged on disk but
        // optimistically unstaged).
        let mut unstaged_hunks = Vec::new();
        let mut index_footprints = Vec::new();
        for range in &worktree_ranges {
            unstaged_hunks.extend(
                unstaged_snapshot.raw_hunks_intersecting_range(range.clone(), &buffer_snapshot),
            );
            index_footprints.push(
                unstaged_snapshot.base_text_range_for_buffer_range(range.clone(), &buffer_snapshot),
            );
        }
        unstaged_hunks.sort_by_key(|hunk| hunk.buffer_range.start.to_offset(&buffer_snapshot));
        unstaged_hunks.dedup_by(|a, b| a.buffer_range.start == b.buffer_range.start);

        // The uncommitted hunks the region covers get the optimistic
        // "staging" secondary status (free cross-view update).
        let uncommitted_diff = self.get_uncommitted_diff(buffer_id, cx);
        let mut uncommitted_hunks = Vec::new();
        if let Some(uncommitted_diff) = &uncommitted_diff {
            let uncommitted_snapshot = uncommitted_diff.read(cx).snapshot(cx);
            for range in &worktree_ranges {
                uncommitted_hunks.extend(
                    uncommitted_snapshot
                        .hunks_intersecting_range(range.clone(), &buffer_snapshot)
                        .filter(|hunk| {
                            hunk.secondary_status != DiffHunkSecondaryStatus::NoSecondaryHunk
                        }),
                );
            }
            uncommitted_hunks
                .sort_by_key(|hunk| hunk.buffer_range.start.to_offset(&buffer_snapshot));
            uncommitted_hunks.dedup_by(|a, b| a.buffer_range.start == b.buffer_range.start);
        }

        let index_edits = if !file_exists {
            // The worktree file is gone: staging removes it from the index.
            None
        } else {
            Some(
                unstaged_hunks
                    .iter()
                    .map(|hunk| {
                        let worktree_range = hunk.buffer_range.to_offset(&buffer_snapshot);
                        let replacement: Arc<str> = Arc::from(
                            buffer_snapshot
                                .text_for_range(worktree_range)
                                .collect::<String>(),
                        );
                        (hunk.diff_base_byte_range.clone(), replacement)
                    })
                    .collect::<Vec<_>>(),
            )
        };

        let version = buffer_snapshot.version().clone();
        let unstaged_pending = pending_hunks(&unstaged_hunks, &version, PendingSense::Suppress);
        let uncommitted_pending = pending_hunks(
            &uncommitted_hunks,
            &version,
            PendingSense::SetSecondaryStatus { stage: true },
        );
        drop(unstaged_snapshot);

        let diff_state = self
            .diffs
            .get(&buffer_id)
            .cloned()
            .context("failed to find git state for buffer")?;
        diff_state.update(cx, |diff_state, _| {
            diff_state.remove_overlapping_pending_index_edits(&index_footprints);
            diff_state.insert_pending_index_edits(index_edits);
        });

        if let Some(uncommitted_diff) = uncommitted_diff {
            uncommitted_diff.update(cx, |diff, cx| {
                diff.set_pending_hunks(&uncommitted_pending, &buffer_snapshot, cx);
            });
        }
        unstaged_diff.update(cx, |diff, cx| {
            diff.set_pending_hunks(&unstaged_pending, &buffer_snapshot, cx);
        });

        self.write_optimistic_index(buffer_id, cx);
        Ok(())
    }

    /// Unstages the worktree changes covered by `worktree_ranges`, acting on the
    /// given uncommitted (HEAD-vs-worktree) diff, invoked from the uncommitted
    /// (gutter) controls. Uses the worktree->index projection (the hard part)
    /// because the acted-on hunks are HEAD-vs-worktree.
    pub fn unstage_uncommitted_hunks(
        &mut self,
        buffer: Entity<Buffer>,
        uncommitted_diff: Entity<BufferDiff>,
        worktree_ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if worktree_ranges.is_empty() {
            return Ok(());
        }
        let buffer_snapshot = buffer.read(cx).snapshot();
        let buffer_id = buffer_snapshot.remote_id();
        let file_exists = buffer_snapshot
            .file()
            .is_some_and(|file| file.disk_state().exists());

        let uncommitted_snapshot = uncommitted_diff.read(cx).snapshot(cx);
        let unstaged_snapshot = uncommitted_snapshot
            .secondary_diff()
            .context("diff has no unstaged secondary")?;

        let mut hunks = Vec::new();
        for range in &worktree_ranges {
            hunks.extend(
                uncommitted_snapshot.hunks_intersecting_range(range.clone(), &buffer_snapshot),
            );
        }
        hunks.sort_by_key(|hunk| hunk.buffer_range.start.to_offset(&buffer_snapshot));
        hunks.dedup_by(|a, b| a.buffer_range.start == b.buffer_range.start);

        let (index_edits, pending) = uncommitted_snapshot.compute_uncommitted_index_edits(
            unstaged_snapshot,
            false,
            &hunks,
            &buffer_snapshot,
            file_exists,
        );
        drop(uncommitted_snapshot);

        let diff_state = self
            .diffs
            .get(&buffer_id)
            .cloned()
            .context("failed to find git state for buffer")?;
        diff_state.update(cx, |diff_state, _| {
            diff_state.insert_pending_index_edits(index_edits)
        });

        uncommitted_diff.update(cx, |diff, cx| {
            diff.set_pending_hunks(&pending, &buffer_snapshot, cx);
        });

        self.write_optimistic_index(buffer_id, cx);
        Ok(())
    }

    /// Unstages staged (HEAD-vs-index) hunks covered by `index_ranges` (in the
    /// index text buffer's coordinates), acting on the given staged diff,
    /// invoked from the staged-changes view. The acted-on hunks already carry
    /// an index range, so no projection is needed; optimistically suppresses
    /// them from the staged diff.
    pub fn unstage_staged_hunks(
        &mut self,
        staged_diff: Entity<BufferDiff>,
        index_ranges: Vec<Range<Anchor>>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if index_ranges.is_empty() {
            return Ok(());
        }
        let index_buffer_id = staged_diff.read(cx).buffer_id;
        let buffer_id = *self
            .buffer_ids_by_index_text_buffer_id
            .get(&index_buffer_id)
            .context("failed to find git state for index text buffer")?;
        let diff_state = self
            .diffs
            .get(&buffer_id)
            .cloned()
            .context("failed to find git state for buffer")?;
        let index_buffer = diff_state
            .read(cx)
            .index_text_buffer()
            .context("index text is not loaded")?;
        let index_snapshot = index_buffer.read(cx).text_snapshot();
        let staged_snapshot = staged_diff.read(cx).snapshot(cx);

        let mut hunks = Vec::new();
        for range in &index_ranges {
            hunks.extend(staged_snapshot.hunks_intersecting_range(range.clone(), &index_snapshot));
        }
        hunks.sort_by_key(|hunk| hunk.buffer_range.start.to_offset(&index_snapshot));
        hunks.dedup_by(|a, b| a.buffer_range.start == b.buffer_range.start);

        let index_edits = staged_diff
            .read(cx)
            .unstage_staged_hunks(&hunks, &index_snapshot);
        let version = index_snapshot.version().clone();
        let pending = pending_hunks(&hunks, &version, PendingSense::Suppress);
        drop(staged_snapshot);

        diff_state.update(cx, |diff_state, _| {
            diff_state.insert_pending_index_edits(index_edits)
        });
        staged_diff.update(cx, |diff, cx| {
            diff.set_pending_hunks(&pending, &index_snapshot, cx);
        });

        self.write_optimistic_index(buffer_id, cx);
        Ok(())
    }

    /// Derives the desired index text from the buffer's optimistic patch and
    /// schedules the write.
    fn write_optimistic_index(&mut self, buffer_id: BufferId, cx: &mut Context<Self>) {
        let Some(diff_state) = self.diffs.get(&buffer_id) else {
            return;
        };
        let new_index_text = diff_state
            .read(cx)
            .pending_index_text(cx)
            .map(|rope| rope.to_string());
        self.write_index_text_for_buffer_id(buffer_id, new_index_text, cx);
    }

    pub fn open_diff_since(
        &mut self,
        oid: Option<git::Oid>,
        buffer: Entity<Buffer>,
        repo: Entity<Repository>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(diff_state) = self.diffs.get(&buffer_id)
            && let Some(oid_diff) = diff_state.read(cx).oid_diff(oid)
        {
            if let Some(task) =
                diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
            {
                return cx.background_executor().spawn(async move {
                    task.await;
                    Ok(oid_diff)
                });
            }
            return Task::ready(Ok(oid_diff));
        }

        let diff_kind = DiffKind::SinceOid(oid);
        if let Some(task) = self.loading_diffs.get(&(buffer_id, diff_kind)) {
            let task = task.clone();
            return cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) });
        }

        let task = cx
            .spawn(async move |this, cx| {
                let result: Result<Entity<BufferDiff>> = async {
                    let buffer_snapshot = buffer.update(cx, |buffer, _| buffer.snapshot());
                    let language_registry =
                        buffer.update(cx, |buffer, _| buffer.language_registry());
                    let content: Option<Arc<str>> = match oid {
                        None => None,
                        Some(oid) => Some({
                            let mut content = repo
                                .update(cx, |repo, cx| repo.load_blob_content(oid, cx))
                                .await?;
                            text::LineEnding::normalize(&mut content);
                            content.into()
                        }),
                    };
                    let buffer_diff = cx.new(|cx| {
                        BufferDiff::new(
                            &buffer_snapshot,
                            buffer_snapshot.language().cloned(),
                            language_registry,
                            cx,
                        )
                    });

                    buffer_diff
                        .update(cx, |buffer_diff, cx| {
                            buffer_diff.set_base_text(content.clone(), buffer_snapshot.text, cx)
                        })
                        .await;
                    let unstaged_diff = this
                        .update(cx, |this, cx| this.open_unstaged_diff(buffer.clone(), cx))?
                        .await?;
                    buffer_diff.update(cx, |buffer_diff, _| {
                        buffer_diff.set_secondary_diff(unstaged_diff);
                    });

                    this.update(cx, |this, cx| {
                        this.loading_diffs.remove(&(buffer_id, diff_kind));

                        let git_store = cx.weak_entity();
                        let diff_state = this
                            .diffs
                            .entry(buffer_id)
                            .or_insert_with(|| cx.new(|cx| BufferGitState::new(git_store, cx)));

                        diff_state.update(cx, |state, _| {
                            if let Some(oid) = oid {
                                if let Some(content) = content {
                                    state.oid_texts.insert(oid, content);
                                }
                            }
                            state.oid_diffs.insert(oid, buffer_diff.downgrade());
                        });
                    })?;

                    Ok(buffer_diff)
                }
                .await;
                result.map_err(Arc::new)
            })
            .shared();

        self.loading_diffs
            .insert((buffer_id, diff_kind), task.clone());
        cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    /// Opens the diff that editors display in the gutter: the uncommitted diff
    /// when the diff base is HEAD, otherwise a diff against the base blob.
    pub fn open_display_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let mut branch_base = None;
        if self.diff_base == GitDiffBaseSetting::DefaultBranch
            && let Some((repo, repo_path)) =
                self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
            && let Some((buffers, _)) = self
                .display_diffs
                .get(&repo.read(cx).id)
                .and_then(|state| state.buffers.as_ref())
        {
            let buffers = buffers.read(cx);
            // The inner `None` means the file was added since the base and
            // diffs against empty content.
            let base_oid = buffers.base_oid_for_path(&repo_path).or_else(|| {
                buffers
                    .status_for_path(&repo_path, cx)
                    .is_some_and(FileStatus::is_created)
                    .then_some(None)
            });
            if let Some(oid) = base_oid {
                branch_base = Some((repo, oid));
            }
        }
        match branch_base {
            Some((repo, oid)) => self.open_diff_since(oid, buffer, repo, cx),
            None => self.open_uncommitted_diff(buffer, cx),
        }
    }

    #[ztracing::instrument(skip_all)]
    pub fn open_uncommitted_diff(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<BufferDiff>>> {
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(diff_state) = self.diffs.get(&buffer_id)
            && let Some(uncommitted_diff) = diff_state
                .read(cx)
                .uncommitted_diff
                .as_ref()
                .and_then(|weak| weak.upgrade())
        {
            if let Some(task) =
                diff_state.update(cx, |diff_state, _| diff_state.wait_for_recalculation())
            {
                return cx.background_executor().spawn(async move {
                    task.await;
                    Ok(uncommitted_diff)
                });
            }
            return Task::ready(Ok(uncommitted_diff));
        }

        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.read(cx).remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find git repository for buffer")));
        };

        let is_symlink = Self::buffer_is_symlink(&buffer, cx);
        let task = self
            .loading_diffs
            .entry((buffer_id, DiffKind::Uncommitted))
            .or_insert_with(|| {
                let changes = if is_symlink {
                    Task::ready(Ok(DiffBasesChange::SetBoth(None)))
                } else {
                    repo.update(cx, |repo, cx| {
                        repo.load_committed_text(buffer_id, repo_path, cx)
                    })
                };

                // todo(lw): hot foreground spawn
                cx.spawn(async move |this, cx| {
                    Self::open_diff_internal(this, DiffKind::Uncommitted, changes.await, buffer, cx)
                        .await
                        .map_err(Arc::new)
                })
                .shared()
            })
            .clone();

        cx.background_spawn(async move { task.await.map_err(|e| anyhow!("{e}")) })
    }

    #[ztracing::instrument(skip_all)]
    async fn open_diff_internal(
        this: WeakEntity<Self>,
        kind: DiffKind,
        texts: Result<DiffBasesChange>,
        buffer_entity: Entity<Buffer>,
        cx: &mut AsyncApp,
    ) -> Result<Entity<BufferDiff>> {
        let diff_bases_change = match texts {
            Err(e) => {
                this.update(cx, |this, cx| {
                    let buffer = buffer_entity.read(cx);
                    let buffer_id = buffer.remote_id();
                    this.loading_diffs.remove(&(buffer_id, kind));
                })?;
                return Err(e);
            }
            Ok(change) => change,
        };

        this.update(cx, |this, cx| {
            let buffer = buffer_entity.read(cx);
            let buffer_id = buffer.remote_id();
            let language = buffer.language().cloned();
            let language_registry = buffer.language_registry();
            let index_text_file = buffer.file().map(|file| {
                Arc::new(IndexTextFile::new(file.as_ref(), cx)) as Arc<dyn language::File>
            });
            let text_snapshot = buffer.text_snapshot();
            this.loading_diffs.remove(&(buffer_id, kind));

            let git_store = cx.weak_entity();
            let project = this.project.clone();
            let diff_state = this
                .diffs
                .entry(buffer_id)
                .or_insert_with(|| cx.new(|cx| BufferGitState::new(git_store.clone(), cx)));

            let existing_unstaged_diff = diff_state.read(cx).unstaged_diff();

            let mut staged_index_text_buffer = None;
            let diff = if kind == DiffKind::Unstaged
                && let Some(existing_unstaged_diff) = existing_unstaged_diff.clone()
            {
                existing_unstaged_diff
            } else {
                let diff = match kind {
                    DiffKind::Unstaged => {
                        let base_text_buffer = diff_state.update(cx, |diff_state, cx| {
                            diff_state.get_or_create_index_text_buffer(index_text_file.clone(), cx)
                        });
                        cx.new(|cx| {
                            let mut diff = BufferDiff::new_with_base_text_buffer(
                                &text_snapshot,
                                base_text_buffer,
                                cx,
                            );
                            if let Some(project) = project.clone() {
                                diff.set_operations(Arc::new(GitDiffOperations { project, kind }));
                            }
                            diff
                        })
                    }
                    DiffKind::Staged => {
                        let (index_text_buffer, base_text_buffer) =
                            diff_state.update(cx, |diff_state, cx| {
                                (
                                    diff_state.get_or_create_index_text_buffer(
                                        index_text_file.clone(),
                                        cx,
                                    ),
                                    diff_state.get_or_create_head_text_buffer(cx),
                                )
                            });
                        index_text_buffer.update(cx, |index_text_buffer, cx| {
                            if let Some(language_registry) = language_registry.clone() {
                                index_text_buffer.set_language_registry(language_registry);
                            }
                            index_text_buffer.set_language_async(language.clone(), cx);
                        });
                        let index_text_snapshot = index_text_buffer.read(cx).text_snapshot();
                        staged_index_text_buffer = Some(index_text_buffer);
                        cx.new(|cx| {
                            let mut diff = BufferDiff::new_with_base_text_buffer(
                                &index_text_snapshot,
                                base_text_buffer,
                                cx,
                            );
                            if let Some(project) = project.clone() {
                                diff.set_operations(Arc::new(GitDiffOperations { project, kind }));
                            }
                            diff
                        })
                    }
                    DiffKind::Uncommitted => {
                        let base_text_buffer = diff_state.update(cx, |diff_state, cx| {
                            diff_state.get_or_create_head_text_buffer(cx)
                        });
                        cx.new(|cx| {
                            let mut diff = BufferDiff::new_with_base_text_buffer(
                                &text_snapshot,
                                base_text_buffer,
                                cx,
                            );
                            if let Some(project) = project.clone() {
                                diff.set_operations(Arc::new(GitDiffOperations { project, kind }));
                            }
                            diff
                        })
                    }
                    DiffKind::SinceOid(_) => {
                        unreachable!("open_diff_internal is not used for OID diffs")
                    }
                };
                diff
            };
            let rx = diff_state.update(cx, |diff_state, cx| {
                diff_state.language = language;
                diff_state.language_registry = language_registry;

                match kind {
                    DiffKind::Unstaged => {
                        diff_state.unstaged_diff = Some(diff.downgrade());
                        // The deleted (base) side of a standalone unstaged diff
                        // is the index, so highlight it. The recalc kicked off by
                        // `diff_bases_changed` below applies the language.
                        diff_state.index_text_buffer_language_enabled = true;
                    }
                    DiffKind::Staged => {
                        diff_state.index_text_buffer_language_enabled = true;
                        let index_text_buffer = staged_index_text_buffer
                            .take()
                            .context("index text buffer was not created for staged diff")?;
                        diff_state.staged_diff = Some((diff.downgrade(), index_text_buffer));
                    }
                    DiffKind::Uncommitted => {
                        let unstaged_diff = if let Some(diff) = existing_unstaged_diff {
                            diff
                        } else {
                            let base_text_buffer =
                                diff_state.get_or_create_index_text_buffer(index_text_file, cx);
                            let unstaged_diff = cx.new(|cx| {
                                let mut diff = BufferDiff::new_with_base_text_buffer(
                                    &text_snapshot,
                                    base_text_buffer,
                                    cx,
                                );
                                if let Some(project) = project.clone() {
                                    diff.set_operations(Arc::new(GitDiffOperations {
                                        project,
                                        kind: DiffKind::Unstaged,
                                    }));
                                }
                                diff
                            });
                            diff_state.unstaged_diff = Some(unstaged_diff.downgrade());
                            unstaged_diff
                        };

                        diff.update(cx, |diff, _| diff.set_secondary_diff(unstaged_diff));
                        diff_state.uncommitted_diff = Some(diff.downgrade())
                    }
                    DiffKind::SinceOid(_) => {
                        unreachable!("open_diff_internal is not used for OID diffs")
                    }
                }

                diff_state.diff_bases_changed(text_snapshot, Some(diff_bases_change), cx);
                let rx = diff_state.wait_for_recalculation();

                anyhow::Ok(async move {
                    if let Some(rx) = rx {
                        rx.await;
                    }
                    Ok(diff)
                })
            });
            let diff_state = this.diffs.get(&buffer_id).cloned();
            if let Some(index_text_buffer) =
                diff_state.and_then(|diff_state| diff_state.read(cx).index_text_buffer())
            {
                this.buffer_ids_by_index_text_buffer_id
                    .insert(index_text_buffer.read(cx).remote_id(), buffer_id);
            }
            rx
        })??
        .await
    }

    pub fn get_unstaged_diff(&self, buffer_id: BufferId, cx: &App) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).unstaged_diff.as_ref()?.upgrade()
    }

    pub fn get_staged_diff(&self, buffer_id: BufferId, cx: &App) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).staged_diff()
    }

    pub fn get_uncommitted_diff(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).uncommitted_diff.as_ref()?.upgrade()
    }

    pub fn get_diff_since_oid(
        &self,
        buffer_id: BufferId,
        oid: Option<git::Oid>,
        cx: &App,
    ) -> Option<Entity<BufferDiff>> {
        let diff_state = self.diffs.get(&buffer_id)?;
        diff_state.read(cx).oid_diff(oid)
    }

    /// Whether this buffer's index text is known to match its committed text
    /// without comparing contents, i.e. whether the texts share one allocation.
    /// In a downstream project, this can only be true if the upstream sent
    /// `Mode::IndexMatchesHead`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn index_matches_head_for_buffer(&self, buffer_id: BufferId, cx: &App) -> bool {
        self.diffs
            .get(&buffer_id)
            .is_some_and(|diff_state| diff_state.read(cx).index_matches_head())
    }

    pub fn open_conflict_set(
        &mut self,
        buffer: Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Entity<ConflictSet>> {
        let buffer_id = buffer.read(cx).remote_id();

        if let Some(git_state) = self.diffs.get(&buffer_id)
            && let Some(conflict_set) = git_state
                .read(cx)
                .conflict_set
                .as_ref()
                .and_then(|weak| weak.upgrade())
        {
            let conflict_set = conflict_set;
            let buffer_snapshot = buffer.read(cx).text_snapshot();

            let rx = git_state.update(cx, |state, cx| {
                state.reparse_conflict_markers(buffer_snapshot, cx)
            });

            return cx.spawn(async move |_, _| {
                rx.await.ok();
                conflict_set
            });
        }

        let is_unmerged = self
            .repository_and_path_for_buffer_id(buffer_id, cx)
            .is_some_and(|(repo, path)| repo.read(cx).snapshot.has_conflict(&path));
        let git_store = cx.weak_entity();
        let buffer_git_state = self
            .diffs
            .entry(buffer_id)
            .or_insert_with(|| cx.new(|cx| BufferGitState::new(git_store, cx)));
        let conflict_set = cx.new(|cx| ConflictSet::new(buffer_id, is_unmerged, cx));

        self._subscriptions
            .push(cx.subscribe(&conflict_set, |_, _, _, cx| {
                cx.emit(GitStoreEvent::ConflictsUpdated);
            }));

        let rx = buffer_git_state.update(cx, |state, cx| {
            state.conflict_set = Some(conflict_set.downgrade());
            let buffer_snapshot = buffer.read(cx).text_snapshot();
            state.reparse_conflict_markers(buffer_snapshot, cx)
        });

        cx.spawn(async move |_, _| {
            rx.await.ok();
            conflict_set
        })
    }

    pub fn project_path_git_status(
        &self,
        project_path: &ProjectPath,
        cx: &App,
    ) -> Option<FileStatus> {
        let (repo, repo_path) = self.repository_and_path_for_project_path(project_path, cx)?;
        Some(repo.read(cx).status_for_path(&repo_path)?.status)
    }

    pub fn checkpoint(&self, cx: &mut App) -> Task<Result<GitStoreCheckpoint>> {
        let mut work_directory_abs_paths = Vec::new();
        let mut checkpoints = Vec::new();
        for repository in self.repositories.values() {
            repository.update(cx, |repository, _| {
                work_directory_abs_paths.push(repository.snapshot.work_directory_abs_path.clone());
                checkpoints.push(repository.checkpoint().map(|checkpoint| checkpoint?));
            });
        }

        cx.background_executor().spawn(async move {
            let checkpoints = future::try_join_all(checkpoints).await?;
            Ok(GitStoreCheckpoint {
                checkpoints_by_work_dir_abs_path: work_directory_abs_paths
                    .into_iter()
                    .zip(checkpoints)
                    .collect(),
            })
        })
    }

    pub fn restore_checkpoint(
        &self,
        checkpoint: GitStoreCheckpoint,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let repositories_by_work_dir_abs_path = self
            .repositories
            .values()
            .map(|repo| (repo.read(cx).snapshot.work_directory_abs_path.clone(), repo))
            .collect::<HashMap<_, _>>();

        let mut tasks = Vec::new();
        for (work_dir_abs_path, checkpoint) in checkpoint.checkpoints_by_work_dir_abs_path {
            if let Some(repository) = repositories_by_work_dir_abs_path.get(&work_dir_abs_path) {
                let restore = repository.update(cx, |repository, _| {
                    repository.restore_checkpoint(checkpoint)
                });
                tasks.push(async move { restore.await? });
            }
        }
        cx.background_spawn(async move {
            future::try_join_all(tasks).await?;
            Ok(())
        })
    }

    /// Compares two checkpoints, returning true if they are equal.
    pub fn compare_checkpoints(
        &self,
        left: GitStoreCheckpoint,
        mut right: GitStoreCheckpoint,
        cx: &mut App,
    ) -> Task<Result<bool>> {
        let repositories_by_work_dir_abs_path = self
            .repositories
            .values()
            .map(|repo| (repo.read(cx).snapshot.work_directory_abs_path.clone(), repo))
            .collect::<HashMap<_, _>>();

        let mut tasks = Vec::new();
        for (work_dir_abs_path, left_checkpoint) in left.checkpoints_by_work_dir_abs_path {
            if let Some(right_checkpoint) = right
                .checkpoints_by_work_dir_abs_path
                .remove(&work_dir_abs_path)
            {
                if let Some(repository) = repositories_by_work_dir_abs_path.get(&work_dir_abs_path)
                {
                    let compare = repository.update(cx, |repository, _| {
                        repository.compare_checkpoints(left_checkpoint, right_checkpoint)
                    });

                    tasks.push(async move { compare.await? });
                }
            } else {
                return Task::ready(Ok(false));
            }
        }
        cx.background_spawn(async move {
            Ok(future::try_join_all(tasks)
                .await?
                .into_iter()
                .all(|result| result))
        })
    }

    /// Blames a buffer.
    pub fn blame_buffer(
        &self,
        buffer: &Entity<Buffer>,
        version: Option<clock::Global>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Blame>>> {
        let buffer = buffer.read(cx);
        let Some((repo, repo_path)) =
            self.repository_and_path_for_buffer_id(buffer.remote_id(), cx)
        else {
            return Task::ready(Err(anyhow!("failed to find a git repository for buffer")));
        };
        let content = match &version {
            Some(version) => buffer.rope_for_version(version),
            None => buffer.as_rope().clone(),
        };
        let line_ending = buffer.line_ending();
        let version = version.unwrap_or(buffer.version());
        let buffer_id = buffer.remote_id();

        let repo = repo.downgrade();
        cx.spawn(async move |_, cx| {
            let repository_state = repo
                .update(cx, |repo, _| repo.repository_state.clone())?
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
            match repository_state {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => backend
                    .blame(repo_path.clone(), content, line_ending)
                    .await
                    .with_context(|| format!("Failed to blame {:?}", repo_path.as_ref()))
                    .map(Some),
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::BlameBuffer {
                            project_id: project_id.to_proto(),
                            buffer_id: buffer_id.into(),
                            version: serialize_version(&version),
                        })
                        .await?;
                    Ok(deserialize_blame_buffer_response(response))
                }
            }
        })
    }

    pub fn get_permalink_to_line(
        &self,
        buffer: &Entity<Buffer>,
        selection: Range<u32>,
        cx: &mut App,
    ) -> Task<Result<url::Url>> {
        let Some(file) = File::from_dyn(buffer.read(cx).file()) else {
            return Task::ready(Err(anyhow!("buffer has no file")));
        };

        let project_path = ProjectPath {
            worktree_id: file.worktree.read(cx).id(),
            path: file.path.clone(),
        };

        if let Some((repo, repo_path)) =
            self.repository_and_path_for_project_path(&project_path, cx)
        {
            return self.build_permalink(
                repo,
                repo_path,
                PermalinkTarget::Buffer {
                    buffer_id: buffer.read(cx).remote_id(),
                    selection,
                },
                cx,
            );
        }

        // If we're not in a Git repo, check whether this is a Rust source
        // file in the Cargo registry (presumably opened with go-to-definition
        // from a normal Rust file). If so, we can put together a permalink
        // using crate metadata.
        if buffer
            .read(cx)
            .language()
            .is_none_or(|lang| lang.name() != "Rust")
        {
            return Task::ready(Err(anyhow!("no permalink available")));
        }
        let file_path = file.worktree.read(cx).absolutize(&file.path);
        cx.spawn(async move |cx| {
            let provider_registry = cx.update(GitHostingProviderRegistry::default_global);
            get_permalink_in_rust_registry_src(provider_registry, file_path, selection)
                .context("no permalink available")
        })
    }

    pub fn get_file_permalink(
        &self,
        project_path: &ProjectPath,
        cx: &mut App,
    ) -> Task<Result<url::Url>> {
        let Some((repo, repo_path)) = self.repository_and_path_for_project_path(project_path, cx)
        else {
            return Task::ready(Err(anyhow!("no permalink available")));
        };
        self.build_permalink(
            repo,
            repo_path,
            PermalinkTarget::File(project_path.clone()),
            cx,
        )
    }

    fn build_permalink(
        &self,
        repo: Entity<Repository>,
        repo_path: RepoPath,
        target: PermalinkTarget,
        cx: &mut App,
    ) -> Task<Result<url::Url>> {
        let branch = repo.read(cx).branch.clone();
        let remote = branch
            .as_ref()
            .and_then(|b| b.upstream.as_ref())
            .and_then(|b| b.remote_name())
            .unwrap_or("origin")
            .to_string();

        let rx = repo.update(cx, |repo, _| {
            repo.send_job("get_permalink", None, move |state, cx| async move {
                match state {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        let origin_url = backend
                            .remote_url(&remote)
                            .await
                            .with_context(|| format!("remote \"{remote}\" not found"))?;

                        let sha = backend.head_sha().await.context("reading HEAD SHA")?;

                        let provider_registry =
                            cx.update(GitHostingProviderRegistry::default_global);

                        let (provider, remote) =
                            parse_git_remote_url(provider_registry, &origin_url)
                                .context("parsing Git remote URL")?;

                        Ok(provider.build_permalink(
                            remote,
                            BuildPermalinkParams::new(&sha, &repo_path, target.selection()),
                        ))
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let permalink = match target {
                            PermalinkTarget::Buffer {
                                buffer_id,
                                selection,
                            } => {
                                client
                                    .request(proto::GetPermalinkToLine {
                                        project_id: project_id.to_proto(),
                                        buffer_id: buffer_id.into(),
                                        selection: Some(proto::Range {
                                            start: selection.start as u64,
                                            end: selection.end as u64,
                                        }),
                                    })
                                    .await?
                                    .permalink
                            }
                            PermalinkTarget::File(project_path) => {
                                client
                                    .request(proto::GetFilePermalink {
                                        project_id: project_id.to_proto(),
                                        path: Some(project_path.to_proto()),
                                    })
                                    .await?
                                    .permalink
                            }
                        };

                        url::Url::parse(&permalink).context("failed to parse permalink")
                    }
                }
            })
        });
        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    fn downstream_client(&self) -> Option<(AnyProtoClient, ProjectId)> {
        match &self.state {
            GitStoreState::Local {
                downstream: downstream_client,
                ..
            } => downstream_client
                .as_ref()
                .map(|state| (state.client.clone(), state.project_id)),
            GitStoreState::Remote {
                downstream: downstream_client,
                ..
            } => downstream_client.clone(),
        }
    }

    fn upstream_client(&self) -> Option<AnyProtoClient> {
        match &self.state {
            GitStoreState::Local { .. } => None,
            GitStoreState::Remote {
                upstream_client, ..
            } => Some(upstream_client.clone()),
        }
    }

    fn on_worktree_store_event(
        &mut self,
        worktree_store: Entity<WorktreeStore>,
        event: &WorktreeStoreEvent,
        cx: &mut Context<Self>,
    ) {
        let GitStoreState::Local {
            project_environment,
            downstream,
            fs,
            ..
        } = &self.state
        else {
            return;
        };

        match event {
            WorktreeStoreEvent::WorktreeAdded(worktree) => {
                let worktree = worktree.clone();
                self.track_initial_scan(&worktree, cx);
            }
            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, updated_entries) => {
                if let Some(worktree) = self
                    .worktree_store
                    .read(cx)
                    .worktree_for_id(*worktree_id, cx)
                {
                    let paths_by_git_repo =
                        self.process_updated_entries(&worktree, updated_entries, cx);
                    let downstream = downstream
                        .as_ref()
                        .map(|downstream| downstream.updates_tx.clone());
                    cx.spawn(async move |_, cx| {
                        let paths_by_git_repo = paths_by_git_repo.await;
                        for (repo, paths) in paths_by_git_repo {
                            repo.update(cx, |repo, cx| {
                                repo.paths_changed(paths, downstream.clone(), cx);
                            });
                        }
                    })
                    .detach();
                }
            }
            WorktreeStoreEvent::WorktreeUpdatedGitRepositories(worktree_id, changed_repos) => {
                let Some(worktree) = worktree_store.read(cx).worktree_for_id(*worktree_id, cx)
                else {
                    return;
                };
                log::debug!("received worktree update for repositories: {changed_repos:?}");
                self.update_repositories_from_worktree(
                    *worktree_id,
                    project_environment.clone(),
                    downstream
                        .as_ref()
                        .map(|downstream| downstream.updates_tx.clone()),
                    changed_repos.clone(),
                    fs.clone(),
                    cx,
                );
                self.local_worktree_git_repos_changed(worktree, changed_repos, cx);
            }
            WorktreeStoreEvent::WorktreeRemoved(_entity_id, worktree_id) => {
                self.parked_repositories
                    .retain(|parked| parked.worktree_id != *worktree_id);
                let repos_without_worktree: Vec<RepositoryId> = self
                    .worktree_ids
                    .iter_mut()
                    .filter_map(|(repo_id, worktree_ids)| {
                        worktree_ids.remove(worktree_id);
                        if worktree_ids.is_empty() {
                            Some(*repo_id)
                        } else {
                            None
                        }
                    })
                    .collect();
                let is_active_repo_removed = repos_without_worktree
                    .iter()
                    .any(|repo_id| self.active_repo_id == Some(*repo_id));

                for repo_id in repos_without_worktree {
                    self.display_diffs.remove(&repo_id);
                    self.repositories.remove(&repo_id);
                    self.worktree_ids.remove(&repo_id);
                    if let Some(updates_tx) =
                        downstream.as_ref().map(|downstream| &downstream.updates_tx)
                    {
                        updates_tx
                            .unbounded_send(DownstreamUpdate::RemoveRepository(repo_id))
                            .ok();
                    }
                }

                if is_active_repo_removed {
                    if let Some((&repo_id, _)) = self.repositories.iter().next() {
                        self.active_repo_id = Some(repo_id);
                        cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(repo_id)));
                    } else {
                        self.active_repo_id = None;
                        cx.emit(GitStoreEvent::ActiveRepositoryChanged(None));
                    }
                }
            }
            _ => {}
        }
    }
    fn on_repository_event(
        &mut self,
        repo: Entity<Repository>,
        event: &RepositoryEvent,
        cx: &mut Context<Self>,
    ) {
        let id = repo.read(cx).id;
        let repo_snapshot = repo.read(cx).snapshot.clone();
        for (buffer_id, diff) in self.diffs.iter() {
            if let Some((buffer_repo, repo_path)) =
                self.repository_and_path_for_buffer_id(*buffer_id, cx)
                && buffer_repo == repo
            {
                diff.update(cx, |diff, cx| {
                    if let Some(conflict_set) = &diff.conflict_set {
                        let conflict_status_changed =
                            conflict_set.update(cx, |conflict_set, cx| {
                                let has_conflict = repo_snapshot.has_conflict(&repo_path);
                                conflict_set.set_has_conflict(has_conflict, cx)
                            })?;
                        if conflict_status_changed {
                            let buffer_store = self.buffer_store.read(cx);
                            if let Some(buffer) = buffer_store.get(*buffer_id) {
                                let _ = diff
                                    .reparse_conflict_markers(buffer.read(cx).text_snapshot(), cx);
                            }
                        }
                    }
                    anyhow::Ok(())
                })
                .ok();
            }
        }

        if matches!(event, RepositoryEvent::BranchListChanged) {
            self.refresh_diff_base_for_repo(id, cx);
        }

        cx.emit(GitStoreEvent::RepositoryUpdated(
            id,
            event.clone(),
            self.active_repo_id == Some(id),
        ))
    }

    fn on_jobs_updated(&mut self, _: Entity<Repository>, _: &JobsUpdated, cx: &mut Context<Self>) {
        cx.emit(GitStoreEvent::JobsUpdated)
    }

    fn repository_is_trusted(&self, repository_id: RepositoryId, cx: &mut Context<Self>) -> bool {
        let Some(worktree_ids) = self.worktree_ids.get(&repository_id) else {
            return false;
        };
        let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
            return false;
        };

        worktree_ids.iter().any(|worktree_id| {
            trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                trusted_worktrees.can_trust(&self.worktree_store, *worktree_id, cx)
            })
        })
    }

    /// Update our list of repositories and schedule git scans in response to a notification from a worktree,
    fn update_repositories_from_worktree(
        &mut self,
        worktree_id: WorktreeId,
        project_environment: Entity<ProjectEnvironment>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        updated_git_repositories: UpdatedGitRepositoriesSet,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) {
        let mut removed_ids = Vec::new();

        let is_trusted = TrustedWorktrees::try_get_global(cx)
            .map(|trusted_worktrees| {
                trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.can_trust(&self.worktree_store, worktree_id, cx)
                })
            })
            .unwrap_or(false);

        if self.worktrees_in_initial_scan.contains(&worktree_id) {
            let mut opened_by_user = self.repositories_opened_by_user.borrow_mut();
            for update in updated_git_repositories.iter() {
                if let Some(work_directory_abs_path) = &update.new_work_directory_abs_path {
                    opened_by_user.insert(work_directory_abs_path.clone());
                }
            }
        }

        let mut sorted_updates = updated_git_repositories.iter().collect::<Vec<_>>();
        sorted_updates.sort_by_key(|update| {
            update
                .new_work_directory_abs_path
                .as_ref()
                .map(|work_directory_abs_path| work_directory_abs_path.components().count())
        });
        for update in sorted_updates {
            self.retarget_parked_repositories(update);

            if let Some((repo_id, existing)) = self
                .repositories
                .iter()
                .find(|(_, repo)| {
                    let existing_work_directory_abs_path = &repo.read(cx).work_directory_abs_path;
                    Some(existing_work_directory_abs_path)
                        == update.old_work_directory_abs_path.as_ref()
                        || Some(existing_work_directory_abs_path)
                            == update.new_work_directory_abs_path.as_ref()
                })
                .map(|(id, repo)| (*id, repo.clone()))
            {
                if let Some(new_work_directory_abs_path) =
                    update.new_work_directory_abs_path.clone()
                {
                    let mut merged_worktree_ids = self.take_parked_at(&new_work_directory_abs_path);
                    merged_worktree_ids.insert(worktree_id);
                    self.worktree_ids
                        .entry(repo_id)
                        .or_default()
                        .extend(merged_worktree_ids);
                    let path_changed = update.old_work_directory_abs_path.as_ref()
                        != update.new_work_directory_abs_path.as_ref();
                    if path_changed
                        && let Some(dot_git_abs_path) = update.dot_git_abs_path.clone()
                        && let Some(repository_dir_abs_path) =
                            update.repository_dir_abs_path.clone()
                        && let Some(common_dir_abs_path) = update.common_dir_abs_path.clone()
                    {
                        existing.update(cx, |existing, cx| {
                            existing.reinitialize_local_backend(
                                new_work_directory_abs_path,
                                dot_git_abs_path,
                                repository_dir_abs_path,
                                common_dir_abs_path,
                                project_environment.downgrade(),
                                fs.clone(),
                                is_trusted,
                                cx,
                            );
                            existing.schedule_scan(updates_tx.clone(), cx);
                        });
                    } else {
                        let scan = SnapshotScan::for_git_dir_change(update.git_dir_change);
                        existing.update(cx, |existing, cx| {
                            existing.snapshot.work_directory_abs_path = new_work_directory_abs_path;
                            existing.schedule_partial_scan(scan, updates_tx.clone(), cx);
                        });
                    }
                } else {
                    if let Some(worktree_ids) = self.worktree_ids.get_mut(&repo_id) {
                        worktree_ids.remove(&worktree_id);
                        if worktree_ids.is_empty() {
                            removed_ids.push(repo_id);
                        }
                    }
                }
            } else if let UpdatedGitRepository {
                new_work_directory_abs_path: Some(work_directory_abs_path),
                dot_git_abs_path: Some(dot_git_abs_path),
                repository_dir_abs_path: Some(repository_dir_abs_path),
                common_dir_abs_path: Some(common_dir_abs_path),
                ..
            } = update
            {
                let Some(worktree) = self
                    .worktree_store
                    .read(cx)
                    .worktree_for_id(worktree_id, cx)
                else {
                    continue;
                };
                let parked_repository = ParkedRepository {
                    worktree_id,
                    work_directory_abs_path: work_directory_abs_path.clone(),
                    dot_git_abs_path: dot_git_abs_path.clone(),
                    repository_dir_abs_path: repository_dir_abs_path.clone(),
                    common_dir_abs_path: common_dir_abs_path.clone(),
                };
                if self.should_park_repository(&parked_repository, &worktree, cx) {
                    self.parked_repositories.retain(|parked| {
                        parked.worktree_id != parked_repository.worktree_id
                            || parked.work_directory_abs_path
                                != parked_repository.work_directory_abs_path
                    });
                    self.parked_repositories.push(parked_repository);
                } else {
                    self.register_local_repository(parked_repository, cx);
                }
            }
        }

        for id in removed_ids {
            if self.active_repo_id == Some(id) {
                self.active_repo_id = None;
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(None));
            }
            self.display_diffs.remove(&id);
            self.repositories.remove(&id);
            if let Some(updates_tx) = updates_tx.as_ref() {
                updates_tx
                    .unbounded_send(DownstreamUpdate::RemoveRepository(id))
                    .ok();
            }
        }
    }

    fn retarget_parked_repositories(&mut self, update: &UpdatedGitRepository) {
        match (
            &update.old_work_directory_abs_path,
            &update.new_work_directory_abs_path,
        ) {
            (Some(old_work_directory_abs_path), Some(new_work_directory_abs_path))
                if old_work_directory_abs_path != new_work_directory_abs_path =>
            {
                if let (
                    Some(dot_git_abs_path),
                    Some(repository_dir_abs_path),
                    Some(common_dir_abs_path),
                ) = (
                    &update.dot_git_abs_path,
                    &update.repository_dir_abs_path,
                    &update.common_dir_abs_path,
                ) {
                    for worktree_id in self.take_parked_at(old_work_directory_abs_path) {
                        let already_parked = self.parked_repositories.iter().any(|parked| {
                            parked.worktree_id == worktree_id
                                && parked.work_directory_abs_path == *new_work_directory_abs_path
                        });
                        if !already_parked {
                            self.parked_repositories.push(ParkedRepository {
                                worktree_id,
                                work_directory_abs_path: new_work_directory_abs_path.clone(),
                                dot_git_abs_path: dot_git_abs_path.clone(),
                                repository_dir_abs_path: repository_dir_abs_path.clone(),
                                common_dir_abs_path: common_dir_abs_path.clone(),
                            });
                        }
                    }
                } else {
                    self.parked_repositories.retain(|parked| {
                        &parked.work_directory_abs_path != old_work_directory_abs_path
                    });
                }
            }
            (Some(old_work_directory_abs_path), None) => {
                self.parked_repositories.retain(|parked| {
                    &parked.work_directory_abs_path != old_work_directory_abs_path
                });
            }
            _ => {}
        }
    }

    fn should_park_repository(
        &self,
        repository: &ParkedRepository,
        worktree: &Entity<Worktree>,
        cx: &App,
    ) -> bool {
        if !Self::parking_enabled(repository.worktree_id, cx) {
            return false;
        }
        let worktree = worktree.read(cx);
        if worktree.root_repo_common_dir().is_some() {
            return false;
        }
        let deep_enough = match repository
            .work_directory_abs_path
            .strip_prefix(worktree.abs_path())
        {
            Ok(relative_path) => relative_path.components().count() >= MIN_PARKED_REPOSITORY_DEPTH,
            Err(_) => false,
        };
        deep_enough
            && !self.repositories.values().any(|active_repository| {
                repository
                    .work_directory_abs_path
                    .starts_with(&active_repository.read(cx).work_directory_abs_path)
            })
            && !self.buffer_store.read(cx).buffers().any(|buffer| {
                buffer
                    .read(cx)
                    .project_path(cx)
                    .and_then(|project_path| {
                        self.worktree_store.read(cx).absolutize(&project_path, cx)
                    })
                    .is_some_and(|abs_path| {
                        abs_path.starts_with(&repository.work_directory_abs_path)
                    })
            })
    }

    fn parking_enabled(worktree_id: WorktreeId, cx: &App) -> bool {
        let settings_location = SettingsLocation {
            worktree_id,
            path: RelPath::empty(),
        };
        WorktreeSettings::get(Some(settings_location), cx)
            .file_scan_depth
            .is_some()
    }

    fn take_parked_at(&mut self, work_directory_abs_path: &Path) -> HashSet<WorktreeId> {
        let mut worktree_ids = HashSet::default();
        self.parked_repositories.retain(|parked| {
            if parked.work_directory_abs_path.as_ref() == work_directory_abs_path {
                worktree_ids.insert(parked.worktree_id);
                false
            } else {
                true
            }
        });
        worktree_ids
    }

    fn activate_parked_repositories(
        &mut self,
        mut should_activate: impl FnMut(&ParkedRepository) -> bool,
        cx: &mut Context<Self>,
    ) {
        if self.parked_repositories.is_empty() {
            return;
        }
        let (to_activate, to_keep) = mem::take(&mut self.parked_repositories)
            .into_iter()
            .partition::<Vec<_>, _>(|parked| should_activate(parked));
        self.parked_repositories = to_keep;
        for parked in to_activate {
            self.register_local_repository(parked, cx);
        }
    }

    fn activate_parked_repositories_where_parking_disabled(&mut self, cx: &mut Context<Self>) {
        let activatable_worktree_ids = self
            .parked_repositories
            .iter()
            .map(|parked| parked.worktree_id)
            .filter(|worktree_id| !Self::parking_enabled(*worktree_id, cx))
            .collect::<HashSet<_>>();
        if activatable_worktree_ids.is_empty() {
            return;
        }
        self.activate_parked_repositories(
            |parked| activatable_worktree_ids.contains(&parked.worktree_id),
            cx,
        );
    }

    fn register_local_repository(&mut self, repository: ParkedRepository, cx: &mut Context<Self>) {
        if !matches!(self.state, GitStoreState::Local { .. }) {
            return;
        }
        let mut worktree_ids = self.take_parked_at(&repository.work_directory_abs_path);
        worktree_ids.insert(repository.worktree_id);
        if let Some((&existing_id, _)) = self.repositories.iter().find(|(_, existing)| {
            existing.read(cx).work_directory_abs_path == repository.work_directory_abs_path
        }) {
            self.worktree_ids
                .entry(existing_id)
                .or_default()
                .extend(worktree_ids);
            self.activate_parked_repositories_under(&repository.work_directory_abs_path, cx);
            return;
        }
        let GitStoreState::Local {
            next_repository_id,
            downstream,
            project_environment,
            fs,
            ..
        } = &self.state
        else {
            return;
        };
        let updates_tx = downstream
            .as_ref()
            .map(|downstream| downstream.updates_tx.clone());
        let project_environment = project_environment.downgrade();
        let fs = fs.clone();
        let is_trusted = TrustedWorktrees::try_get_global(cx)
            .map(|trusted_worktrees| {
                trusted_worktrees.update(cx, |trusted_worktrees, cx| {
                    trusted_worktrees.can_trust(&self.worktree_store, repository.worktree_id, cx)
                })
            })
            .unwrap_or(false);

        let id = RepositoryId(next_repository_id.fetch_add(1, atomic::Ordering::Release));
        let git_store = cx.weak_entity();
        let blob_read_limiter = self.blob_read_limiter.clone();
        let opened_by_user = self
            .repositories_opened_by_user
            .borrow()
            .contains(&repository.work_directory_abs_path);
        let repo = cx.new(|cx| {
            let mut repo = Repository::local(
                id,
                repository.work_directory_abs_path.clone(),
                repository.repository_dir_abs_path.clone(),
                repository.common_dir_abs_path.clone(),
                repository.dot_git_abs_path.clone(),
                project_environment,
                fs,
                is_trusted,
                git_store,
                blob_read_limiter,
                cx,
            );
            repo.snapshot.opened_by_user = opened_by_user;
            if let Some(updates_tx) = updates_tx.as_ref() {
                // trigger an empty `UpdateRepository` to ensure remote active_repo_id is set correctly
                updates_tx
                    .unbounded_send(DownstreamUpdate::UpdateRepository(repo.snapshot()))
                    .ok();
            }
            repo.schedule_scan(updates_tx, cx);
            repo
        });
        self._subscriptions
            .push(cx.subscribe(&repo, Self::on_repository_event));
        self._subscriptions
            .push(cx.subscribe(&repo, Self::on_jobs_updated));
        self.repositories.insert(id, repo);
        self.worktree_ids.insert(id, worktree_ids);
        cx.emit(GitStoreEvent::RepositoryAdded);
        self.refresh_diff_base_for_repo(id, cx);
        self.active_repo_id.get_or_insert_with(|| {
            cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
            id
        });
        self.activate_parked_repositories_under(&repository.work_directory_abs_path, cx);
    }

    fn activate_parked_repositories_under(
        &mut self,
        work_directory_abs_path: &Path,
        cx: &mut Context<Self>,
    ) {
        self.activate_parked_repositories(
            |parked| {
                parked
                    .work_directory_abs_path
                    .starts_with(work_directory_abs_path)
            },
            cx,
        );
    }

    #[cfg(feature = "test-support")]
    pub fn parked_repositories(&self) -> &[ParkedRepository] {
        &self.parked_repositories
    }

    #[cfg(feature = "test-support")]
    pub fn activate_all_parked_repositories(&mut self, cx: &mut Context<Self>) {
        self.activate_parked_repositories(|_| true, cx);
    }

    fn activate_parked_repositories_for_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) {
        if self.parked_repositories.is_empty() {
            return;
        }
        if let Some(abs_path) = buffer
            .read(cx)
            .project_path(cx)
            .and_then(|project_path| self.worktree_store.read(cx).absolutize(&project_path, cx))
        {
            self.activate_parked_repositories(
                |parked| abs_path.starts_with(&parked.work_directory_abs_path),
                cx,
            );
        }
    }

    fn on_trusted_worktrees_event(
        &mut self,
        _: Entity<TrustedWorktreesStore>,
        event: &TrustedWorktreesEvent,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.state, GitStoreState::Local { .. }) {
            return;
        }

        let (is_trusted, event_paths) = match event {
            TrustedWorktreesEvent::Trusted(_, trusted_paths) => (true, trusted_paths),
            TrustedWorktreesEvent::Restricted(_, restricted_paths) => (false, restricted_paths),
        };

        for (repo_id, worktree_ids) in &self.worktree_ids {
            if worktree_ids
                .iter()
                .any(|worktree_id| event_paths.contains(&PathTrust::Worktree(*worktree_id)))
            {
                if let Some(repo) = self.repositories.get(repo_id) {
                    let repository_state = repo.read(cx).repository_state.clone();
                    cx.background_spawn(async move {
                        if let Ok(RepositoryState::Local(state)) = repository_state.await {
                            state.backend.set_trusted(is_trusted);
                        }
                    })
                    .detach();
                }
            }
        }
    }

    fn on_buffer_store_event(
        &mut self,
        _: Entity<BufferStore>,
        event: &BufferStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferStoreEvent::BufferAdded(buffer) => {
                self.activate_parked_repositories_for_buffer(buffer, cx);
                cx.subscribe(buffer, |this, buffer, event, cx| {
                    if let BufferEvent::LanguageChanged(_) = event {
                        let buffer_id = buffer.read(cx).remote_id();
                        if let Some(diff_state) = this.diffs.get(&buffer_id) {
                            diff_state.update(cx, |diff_state, cx| {
                                diff_state.buffer_language_changed(buffer, cx);
                            });
                        }
                    }
                })
                .detach();
            }
            BufferStoreEvent::SharedBufferClosed(peer_id, buffer_id) => {
                if let Some(diffs) = self.shared_diffs.get_mut(peer_id) {
                    diffs.remove(buffer_id);
                }
            }
            BufferStoreEvent::BufferDropped(buffer_id) => {
                self.diffs.remove(buffer_id);
                self.buffer_ids_by_index_text_buffer_id
                    .retain(|_, main_buffer_id| main_buffer_id != buffer_id);
                for diffs in self.shared_diffs.values_mut() {
                    diffs.remove(buffer_id);
                }
            }
            BufferStoreEvent::BufferChangedFilePath { buffer, .. } => {
                self.activate_parked_repositories_for_buffer(buffer, cx);
                // Whenever a buffer's file path changes, it's possible that the
                // new path is actually a path that is being tracked by a git
                // repository. In that case, we'll want to update the buffer's
                // `BufferDiffState`, in case it already has one.
                let buffer_id = buffer.read(cx).remote_id();
                let diff_state = self.diffs.get(&buffer_id);
                let repo = self.repository_and_path_for_buffer_id(buffer_id, cx);

                if let Some(diff_state) = diff_state
                    && let Some((repo, repo_path)) = repo
                {
                    let buffer = buffer.clone();
                    let diff_state = diff_state.clone();
                    let is_symlink = Self::buffer_is_symlink(&buffer, cx);

                    cx.spawn(async move |_git_store, cx| {
                        async {
                            let diff_bases_change = if is_symlink {
                                DiffBasesChange::SetBoth(None)
                            } else {
                                repo.update(cx, |repo, cx| {
                                    repo.load_committed_text(buffer_id, repo_path, cx)
                                })
                                .await?
                            };

                            diff_state.update(cx, |diff_state, cx| {
                                let buffer_snapshot = buffer.read(cx).text_snapshot();
                                diff_state.diff_bases_changed(
                                    buffer_snapshot,
                                    Some(diff_bases_change),
                                    cx,
                                );
                            });
                            anyhow::Ok(())
                        }
                        .await
                        .log_err();
                    })
                    .detach();
                }
            }
        }
    }

    pub fn recalculate_buffer_diffs(
        &mut self,
        buffers: Vec<Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let mut futures = Vec::new();
        for buffer in buffers {
            if let Some(diff_state) = self.diffs.get_mut(&buffer.read(cx).remote_id()) {
                let buffer = buffer.read(cx).text_snapshot();
                diff_state.update(cx, |diff_state, cx| {
                    diff_state.recalculate_diffs(buffer.clone(), cx);
                    futures.extend(diff_state.wait_for_recalculation().map(FutureExt::boxed));
                });
                futures.push(diff_state.update(cx, |diff_state, cx| {
                    diff_state
                        .reparse_conflict_markers(buffer, cx)
                        .map(|_| {})
                        .boxed()
                }));
            }
        }
        async move {
            futures::future::join_all(futures).await;
        }
    }

    fn write_index_text_for_buffer_id(
        &mut self,
        buffer_id: BufferId,
        new_index_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(diff_state) = self.diffs.get(&buffer_id) else {
            return;
        };
        let hunk_staging_operation_count = diff_state.update(cx, |diff_state, _| {
            diff_state.hunk_staging_operation_count += 1;
            diff_state.hunk_staging_operation_count
        });
        let Some((repo, path)) = self.repository_and_path_for_buffer_id(buffer_id, cx) else {
            return;
        };
        let (encoding, has_bom) = self
            .buffer_store
            .read(cx)
            .get(buffer_id)
            .map(|buffer| {
                let buffer = buffer.read(cx);
                (buffer.encoding(), buffer.has_bom())
            })
            .unwrap_or((encoding_rs::UTF_8, false));
        let recv = repo.update(cx, |repo, cx| {
            log::debug!("hunks changed for {}", path.as_unix_str());
            repo.spawn_set_index_text_job(
                path,
                new_index_text,
                encoding,
                has_bom,
                Some(hunk_staging_operation_count),
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Err(error)) = cx.background_spawn(recv).await {
                this.update(cx, |this, cx| {
                    if let Some(diff_state) = this.diffs.get(&buffer_id).cloned() {
                        diff_state.update(cx, |diff_state, cx| {
                            diff_state.clear_pending_index_edits_and_hunks(cx);
                        });
                    }
                    cx.emit(GitStoreEvent::IndexWriteError(error));
                })
                .ok();
            }
        })
        .detach();
    }

    fn local_worktree_git_repos_changed(
        &mut self,
        worktree: Entity<Worktree>,
        changed_repos: &UpdatedGitRepositoriesSet,
        cx: &mut Context<Self>,
    ) {
        log::debug!("local worktree repos changed");
        debug_assert!(worktree.read(cx).is_local());

        for repository in self.repositories.values() {
            repository.update(cx, |repository, cx| {
                let repo_abs_path = &repository.work_directory_abs_path;
                if changed_repos.iter().any(|update| {
                    update.old_work_directory_abs_path.as_ref() == Some(repo_abs_path)
                        || update.new_work_directory_abs_path.as_ref() == Some(repo_abs_path)
                }) {
                    repository.reload_buffer_diff_bases(cx);
                }
            });
        }
    }

    pub fn repositories(&self) -> &HashMap<RepositoryId, Entity<Repository>> {
        &self.repositories
    }

    /// Returns the main repository working directory for the given worktree.
    /// For normal checkouts this equals the worktree's own path. For linked
    /// worktrees it points back to the main worktree, if one exists. Linked
    /// worktrees attached to a bare repository have no main worktree path.
    pub fn original_repo_path_for_worktree(
        &self,
        worktree_id: WorktreeId,
        cx: &App,
    ) -> Option<Arc<Path>> {
        self.active_repo_id
            .iter()
            .chain(self.worktree_ids.keys())
            .find(|repo_id| {
                self.worktree_ids
                    .get(repo_id)
                    .is_some_and(|ids| ids.contains(&worktree_id))
            })
            .and_then(|repo_id| self.repositories.get(repo_id))
            .and_then(|repo| {
                repo.read(cx)
                    .snapshot()
                    .main_worktree_abs_path()
                    .map(Arc::from)
            })
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        let (repo, path) = self.repository_and_path_for_buffer_id(buffer_id, cx)?;
        let status = repo.read(cx).snapshot.status_for_path(&path)?;
        Some(status.status)
    }

    pub fn repository_and_path_for_buffer_id(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(Entity<Repository>, RepoPath)> {
        let buffer = self.buffer_store.read(cx).get(buffer_id)?;
        let project_path = buffer.read(cx).project_path(cx)?;
        self.repository_and_path_for_project_path(&project_path, cx)
    }

    pub fn repository_and_path_for_project_path(
        &self,
        path: &ProjectPath,
        cx: &App,
    ) -> Option<(Entity<Repository>, RepoPath)> {
        let abs_path = self.worktree_store.read(cx).absolutize(path, cx)?;
        self.repositories
            .values()
            .filter_map(|repo| {
                let repo_path = repo.read(cx).abs_path_to_repo_path(&abs_path)?;
                Some((repo.clone(), repo_path))
            })
            .max_by_key(|(repo, _)| repo.read(cx).work_directory_abs_path.clone())
    }

    pub fn git_init(
        &self,
        path: Arc<Path>,
        fallback_branch_name: String,
        cx: &App,
    ) -> Task<Result<()>> {
        match &self.state {
            GitStoreState::Local { fs, .. } => {
                // Before the repository exists, so that whichever scan finds it finds it opened.
                self.repositories_opened_by_user
                    .borrow_mut()
                    .insert(path.clone());
                let fs = fs.clone();
                cx.background_executor()
                    .spawn(async move { fs.git_init(&path, fallback_branch_name).await })
            }
            GitStoreState::Remote {
                upstream_client,
                upstream_project_id: project_id,
                ..
            } => {
                let client = upstream_client.clone();
                let project_id = *project_id;
                cx.background_executor().spawn(async move {
                    client
                        .request(proto::GitInit {
                            project_id: project_id,
                            abs_path: path.to_string_lossy().into_owned(),
                            fallback_branch_name,
                        })
                        .await?;
                    Ok(())
                })
            }
        }
    }

    pub fn git_clone(
        &self,
        repo: String,
        path: impl Into<Arc<std::path::Path>>,
        cx: &App,
    ) -> Task<Result<()>> {
        let path = path.into();
        match &self.state {
            GitStoreState::Local { fs, .. } => {
                let fs = fs.clone();
                cx.background_executor()
                    .spawn(async move { fs.git_clone(&path, &repo).await })
            }
            GitStoreState::Remote {
                upstream_client,
                upstream_project_id,
                ..
            } => {
                if upstream_client.is_via_collab() {
                    return Task::ready(Err(anyhow!(
                        "Git Clone isn't supported for project guests"
                    )));
                }
                let request = upstream_client.request(proto::GitClone {
                    project_id: *upstream_project_id,
                    abs_path: path.to_string_lossy().into_owned(),
                    remote_repo: repo,
                });

                cx.background_spawn(async move {
                    let result = request.await?;

                    match result.success {
                        true => Ok(()),
                        false => Err(anyhow!("Git Clone failed")),
                    }
                })
            }
        }
    }

    pub fn git_config(&self, path: Arc<Path>, args: Vec<String>, cx: &App) -> Task<Result<String>> {
        match &self.state {
            GitStoreState::Local { fs, .. } => {
                let fs = fs.clone();
                cx.background_executor()
                    .spawn(async move { fs.git_config(&path, args).await })
            }
            GitStoreState::Remote {
                upstream_client, ..
            } => {
                // Prevent running git config commands for collab.
                if upstream_client.is_via_collab() {
                    return Task::ready(Err(anyhow!(
                        "Git Config isn't support for project guests"
                    )));
                }

                // TODO: Implement this for remote repositories.
                Task::ready(Err(anyhow!(
                    "Git Config isn't yet supported for remote projects"
                )))
            }
        }
    }

    async fn handle_update_repository(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::UpdateRepository>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let path_style = this.worktree_store.read(cx).path_style();
            let mut update = envelope.payload;

            let id = RepositoryId::from_proto(update.id);
            let client = this.upstream_client().context("no upstream client")?;
            let is_new = !this.repositories.contains_key(&id);

            let repository_dir_abs_path: Option<Arc<Path>> = update
                .repository_dir_abs_path
                .as_deref()
                .map(|p| Path::new(p).into());
            let common_dir_abs_path: Option<Arc<Path>> = update
                .common_dir_abs_path
                .as_deref()
                .map(|p| Path::new(p).into());

            let mut repo_subscription = None;
            let blob_read_limiter = this.blob_read_limiter.clone();
            let repo = this.repositories.entry(id).or_insert_with(|| {
                let git_store = cx.weak_entity();
                let repo = cx.new(|cx| {
                    Repository::remote(
                        id,
                        Path::new(&update.abs_path).into(),
                        repository_dir_abs_path.clone(),
                        common_dir_abs_path.clone(),
                        path_style,
                        ProjectId(update.project_id),
                        client,
                        git_store,
                        blob_read_limiter,
                        cx,
                    )
                });
                repo_subscription = Some(cx.subscribe(&repo, Self::on_repository_event));
                cx.emit(GitStoreEvent::RepositoryAdded);
                repo
            });
            this._subscriptions.extend(repo_subscription);

            repo.update(cx, {
                let update = update.clone();
                |repo, cx| repo.apply_remote_update(update, cx)
            })?;

            if is_new {
                this.refresh_diff_base_for_repo(id, cx);
                if let Some(connection) = this.remote_git_account_connection().map(str::to_string)
                    && let Some(repo) = this.repositories.get(&id)
                {
                    repo.update(cx, |repo, cx| repo.send_remote_git_account(&connection, cx));
                }
            }

            this.active_repo_id.get_or_insert_with(|| {
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
                id
            });

            if let Some((client, project_id)) = this.downstream_client() {
                update.project_id = project_id.to_proto();
                client.send(update).log_err();
            }
            Ok(())
        })
    }

    async fn handle_remove_repository(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RemoveRepository>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let mut update = envelope.payload;
            let id = RepositoryId::from_proto(update.id);
            this.display_diffs.remove(&id);
            this.repositories.remove(&id);
            if let Some((client, project_id)) = this.downstream_client() {
                update.project_id = project_id.to_proto();
                client.send(update).log_err();
            }
            if this.active_repo_id == Some(id) {
                this.active_repo_id = None;
                cx.emit(GitStoreEvent::ActiveRepositoryChanged(None));
            }
            cx.emit(GitStoreEvent::RepositoryRemoved(id));
        });
        Ok(())
    }

    async fn handle_git_init(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitInit>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let path: Arc<Path> = PathBuf::from(envelope.payload.abs_path).into();
        let name = envelope.payload.fallback_branch_name;
        cx.update(|cx| this.read(cx).git_init(path, name, cx))
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_git_clone(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitClone>,
        cx: AsyncApp,
    ) -> Result<proto::GitCloneResponse> {
        let path: Arc<Path> = PathBuf::from(envelope.payload.abs_path).into();
        let repo_name = envelope.payload.remote_repo;
        let result = cx
            .update(|cx| this.read(cx).git_clone(repo_name, path, cx))
            .await;

        Ok(proto::GitCloneResponse {
            success: result.is_ok(),
        })
    }

    async fn handle_fetch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Fetch>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let fetch_options =
            FetchOptions::from_proto(envelope.payload.remote, envelope.payload.unshallow);
        let askpass_id = envelope.payload.askpass_id;

        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            repository_id,
            askpass_id,
            &mut cx,
        );

        let remote_output = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.fetch(fetch_options, askpass, cx)
            })
            .await??;

        Ok(proto::RemoteMessageResponse {
            stdout: remote_output.stdout,
            stderr: remote_output.stderr,
            forced_without_lease: None,
        })
    }

    async fn handle_push(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Push>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let askpass_id = envelope.payload.askpass_id;
        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            repository_id,
            askpass_id,
            &mut cx,
        );

        // Not `payload.options()`, which hands back `SET_UPSTREAM` for a value this build does not
        // know: a request asking for something newer would push the branch rather than being
        // refused, which is the whole reason `force_without_lease` is a field of its own.
        let proto_options = envelope
            .payload
            .options
            .map(|option| {
                proto::push::PushOptions::try_from(option)
                    .ok()
                    .with_context(|| i18n::t!("Cannot interpret the push option {option}."))
            })
            .transpose()?;
        let options = push_options_from_proto(
            proto_options,
            envelope.payload.force_without_lease.unwrap_or(false),
        );

        let branch_name = envelope.payload.branch_name.into();
        let remote_branch_name = envelope.payload.remote_branch_name.into();
        let remote_name = envelope.payload.remote_name.into();

        let remote_output = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.push(
                    branch_name,
                    remote_branch_name,
                    remote_name,
                    options,
                    askpass,
                    cx,
                )
            })
            .await??;
        // How the client tells a server that read `force_without_lease` from one that never saw
        // the field and quietly kept the lease. A host that is itself a client of somewhere older
        // says nothing either, rather than passing on a promise it couldn't keep.
        let forced_without_lease = matches!(options, Some(PushOptions::ForceWithoutLease))
            && !remote_output.kept_lease_against_request;
        Ok(proto::RemoteMessageResponse {
            stdout: remote_output.output.stdout,
            stderr: remote_output.output.stderr,
            forced_without_lease: forced_without_lease.then_some(true),
        })
    }

    async fn handle_pull(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Pull>,
        mut cx: AsyncApp,
    ) -> Result<proto::RemoteMessageResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let askpass_id = envelope.payload.askpass_id;
        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            repository_id,
            askpass_id,
            &mut cx,
        );

        let branch_name = envelope.payload.branch_name.map(|name| name.into());
        let remote_name = envelope.payload.remote_name.into();
        let rebase = envelope.payload.rebase;

        let remote_message = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.pull(branch_name, remote_name, rebase, askpass, cx)
            })
            .await??;

        Ok(proto::RemoteMessageResponse {
            stdout: remote_message.stdout,
            stderr: remote_message.stderr,
            forced_without_lease: None,
        })
    }

    async fn handle_stage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Stage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(|path| RepoPath::new(&path))
            .collect::<Result<Vec<_>>>()?;

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stage_entries(entries, cx)
            })
            .await?;
        Ok(proto::Ack {})
    }

    async fn handle_unstage(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Unstage>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(|path| RepoPath::new(&path))
            .collect::<Result<Vec<_>>>()?;

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.unstage_entries(entries, cx)
            })
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_stash(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Stash>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let message = envelope.payload.message;

        if envelope.payload.staged.unwrap_or(false) {
            repository_handle
                .update(&mut cx, |repository_handle, cx| {
                    repository_handle.stash_staged(message, cx)
                })
                .await?;
            return Ok(proto::Ack {});
        }

        let entries = envelope
            .payload
            .paths
            .into_iter()
            .map(|path| RepoPath::new(&path))
            .collect::<Result<Vec<_>>>()?;

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stash_entries(entries, message, cx)
            })
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_stash_pop(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::StashPop>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let stash_index = envelope.payload.stash_index.map(|i| i as usize);

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stash_pop(stash_index, cx)
            })
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_stash_apply(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::StashApply>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let stash_index = envelope.payload.stash_index.map(|i| i as usize);

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stash_apply(stash_index, cx)
            })
            .await?;

        Ok(proto::Ack {})
    }

    async fn handle_stash_drop(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::StashDrop>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let stash_index = envelope.payload.stash_index.map(|i| i as usize);

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.stash_drop(stash_index, cx)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_set_index_text(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetIndexText>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let repo_path = RepoPath::from_proto(&envelope.payload.path)?;

        let (encoding, has_bom) = cx.update(|cx| {
            repository_handle
                .read(cx)
                .repo_path_to_project_path(&repo_path, cx)
                .and_then(|project_path| {
                    this.read(cx)
                        .buffer_store
                        .read(cx)
                        .get_by_path(&project_path)
                })
                .map(|buffer| {
                    let buffer = buffer.read(cx);
                    (buffer.encoding(), buffer.has_bom())
                })
                .unwrap_or((encoding_rs::UTF_8, false))
        });

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.spawn_set_index_text_job(
                    repo_path,
                    envelope.payload.text,
                    encoding,
                    has_bom,
                    None,
                    cx,
                )
            })
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_run_hook(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::RunGitHook>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let hook = RunHook::from_proto(envelope.payload.hook).context("invalid hook")?;
        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.run_hook(hook, cx)
            })
            .await??;
        Ok(proto::Ack {})
    }

    /// ADR-F93: on a remote server, the account the client picked for a repository. Only from
    /// the client this store serves as its remote server: a collaborator must not choose who
    /// the host's commits are made as.
    async fn handle_set_git_account_identity(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SetGitAccountIdentity>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        #[cfg(any(test, feature = "test-support"))]
        if cx.update(|cx| cx.has_global::<crate::git_accounts::IgnoreGitAccountIdentityForTests>())
        {
            anyhow::bail!("no handler registered for SetGitAccountIdentity");
        }
        let identity = envelope
            .payload
            .identity
            .map(crate::git_accounts::identity_from_proto)
            .transpose()?
            .map(Arc::new);
        let serves_remote_client = this.read_with(&cx, |this, _| match &this.state {
            GitStoreState::Local { downstream, .. } => downstream.as_ref().is_some_and(
                |downstream| downstream.project_id.0 == proto::REMOTE_SERVER_PROJECT_ID,
            ),
            GitStoreState::Remote { .. } => false,
        });
        anyhow::ensure!(
            serves_remote_client,
            "git accounts are set only by a remote server's client"
        );
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository = Self::repository_for_request(&this, repository_id, &mut cx)?;
        repository
            .update(&mut cx, |repository, cx| {
                repository.identity_from_client = identity;
                repository.apply_git_account(false, cx)
            })
            .await;
        Ok(proto::Ack {})
    }

    async fn handle_commit(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::Commit>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let askpass_id = envelope.payload.askpass_id;

        let askpass = make_remote_delegate(
            this,
            envelope.payload.project_id,
            repository_id,
            askpass_id,
            &mut cx,
        );

        let message = SharedString::from(envelope.payload.message);
        let name = envelope.payload.name.map(SharedString::from);
        let email = envelope.payload.email.map(SharedString::from);
        let options = envelope.payload.options.unwrap_or_default();

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.commit(
                    message,
                    name.zip(email),
                    CommitOptions {
                        amend: options.amend,
                        signoff: options.signoff,
                        allow_empty: options.allow_empty,
                        no_verify: options.no_verify,
                    },
                    askpass,
                    cx,
                )
            })
            .await?
            .map_err(tag_committed_without_signature)?;
        Ok(proto::Ack {})
    }

    async fn handle_get_remotes(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetRemotes>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetRemotesResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branch_name = envelope.payload.branch_name;
        let is_push = envelope.payload.is_push;

        let remotes = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.get_remotes(branch_name, is_push)
            })
            .await??;
        let remote_urls = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.remote_urls()
            })
            .await??;

        Ok(proto::GetRemotesResponse {
            remotes: remotes
                .into_iter()
                .map(|remotes| proto::get_remotes_response::Remote {
                    name: remotes.name.to_string(),
                    url: remote_urls.get(remotes.name.as_ref()).cloned(),
                })
                .collect::<Vec<_>>(),
        })
    }

    async fn handle_get_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitGetWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitWorktreesResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let worktrees = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.worktrees()
            })
            .await??;

        Ok(proto::GitWorktreesResponse {
            worktrees: worktrees
                .into_iter()
                .map(|worktree| worktree_to_proto(&worktree))
                .collect::<Vec<_>>(),
        })
    }

    async fn handle_create_worktree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let directory = PathBuf::from(envelope.payload.directory);
        let name = envelope.payload.name;
        let commit = envelope.payload.commit;
        let use_existing_branch = envelope.payload.use_existing_branch;
        let target = if name.is_empty() {
            CreateWorktreeTarget::Detached { base_sha: commit }
        } else if use_existing_branch {
            CreateWorktreeTarget::ExistingBranch { branch_name: name }
        } else {
            CreateWorktreeTarget::NewBranch {
                branch_name: name,
                base_sha: commit,
            }
        };

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.create_worktree(target, directory)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_remove_worktree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRemoveWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let path = PathBuf::from(envelope.payload.path);
        let force = envelope.payload.force;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.remove_worktree(path, force)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_rename_worktree(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRenameWorktree>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let old_path = PathBuf::from(envelope.payload.old_path);
        let new_path = PathBuf::from(envelope.payload.new_path);

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.rename_worktree(old_path, new_path)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_worktree_created_at(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitWorktreeCreatedAt>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitWorktreeCreatedAtResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let worktree_path = PathBuf::from(envelope.payload.worktree_path);

        let created_at = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.worktree_created_at(worktree_path)
            })
            .await??;

        Ok(proto::GitWorktreeCreatedAtResponse {
            created_at: created_at.map(Into::into),
        })
    }

    async fn handle_get_head_sha(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitGetHeadSha>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitGetHeadShaResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let head_sha = repository_handle
            .update(&mut cx, |repository_handle, _| repository_handle.head_sha())
            .await??;

        Ok(proto::GitGetHeadShaResponse { sha: head_sha })
    }

    async fn handle_get_commit_data(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetCommitData>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetCommitDataResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let shas: Vec<Oid> = envelope
            .payload
            .shas
            .iter()
            .filter_map(|s| Oid::from_str(s).ok())
            .collect();

        let mut commits = Vec::with_capacity(shas.len());
        let mut receivers = Vec::new();

        repository_handle.update(&mut cx, |repository, cx| {
            for &sha in &shas {
                match repository.fetch_commit_data(sha, true, cx) {
                    CommitDataState::Loaded(data) => {
                        commits.push(commit_data_to_proto(data));
                    }
                    CommitDataState::Loading(Some(shared)) => {
                        receivers.push(shared.clone());
                    }
                    CommitDataState::Loading(None) => {
                        // todo(git_graph) this could happen if the request fails, we should encode an error case
                        debug_panic!(
                            "This should never happen since we passed true into fetch commit data"
                        );
                    }
                }
            }
        });

        let results = future::join_all(receivers).await;

        commits.extend(
            results
                .into_iter()
                .filter_map(|result| result.ok())
                .map(|data| commit_data_to_proto(&data)),
        );

        Ok(proto::GetCommitDataResponse { commits })
    }

    async fn handle_get_initial_graph_data(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetInitialGraphData>,
        mut cx: AsyncApp,
    ) -> Result<impl Stream<Item = Result<proto::GetInitialGraphDataResponse>>> {
        const CHUNK_SIZE: usize = git::repository::GRAPH_CHUNK_SIZE;
        let payload = envelope.payload;

        let repository_id = RepositoryId::from_proto(payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let log_order = log_order_from_proto(payload.log_order());
        let log_source = log_source_from_proto(
            payload
                .log_source
                .context("missing initial graph data log source")?,
        )?;

        let (subscriber_sender, subscriber_receiver) = async_channel::unbounded();
        let (cached_commits, error, is_loading) =
            repository_handle.update(&mut cx, |repository, cx| {
                let response =
                    repository.graph_data(log_source.clone(), log_order, 0..usize::MAX, cx);
                let cached_commits = response.commits.to_vec();
                let error = response.error.clone();
                let is_loading = response.is_loading;

                if is_loading {
                    if let Some(graph_data) = repository
                        .initial_graph_data
                        .get_mut(&(log_source.clone(), log_order))
                    {
                        graph_data.subscribers.push(subscriber_sender);
                    }
                }

                (cached_commits, error, is_loading)
            });

        let (mut response_tx, response_rx) = mpsc::unbounded();
        cx.background_spawn(async move {
            if let Some(error) = error {
                if response_tx
                    .send(Err(anyhow!(error.to_string())))
                    .await
                    .is_err()
                {
                    return;
                }
                return;
            }

            for commits in cached_commits.chunks(CHUNK_SIZE) {
                let response = proto::GetInitialGraphDataResponse {
                    commits: commits
                        .iter()
                        .map(|commit| initial_graph_commit_to_proto(commit))
                        .collect(),
                };
                if response_tx.send(Ok(response)).await.is_err() {
                    return;
                }
            }

            if !is_loading {
                return;
            }

            while let Ok(chunk_result) = subscriber_receiver.recv().await {
                let commits = match chunk_result {
                    Ok(commits) => commits,
                    Err(error) => {
                        response_tx
                            .send(Err(anyhow!(error.to_string())))
                            .await
                            .context("Failed to send error")
                            .log_err();
                        return;
                    }
                };

                for commits in commits.chunks(CHUNK_SIZE) {
                    let response = proto::GetInitialGraphDataResponse {
                        commits: commits
                            .iter()
                            .map(|commit| initial_graph_commit_to_proto(commit))
                            .collect(),
                    };
                    if response_tx.send(Ok(response)).await.is_err() {
                        return;
                    }
                }
            }
        })
        .detach();

        Ok(response_rx)
    }

    async fn handle_search_commits(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SearchCommits>,
        mut cx: AsyncApp,
    ) -> Result<impl Stream<Item = Result<proto::SearchCommitsResponse>>> {
        const CHUNK_SIZE: usize = 100;

        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let search_args = search_commit_args_from_proto(&envelope.payload)?;
        let sender_id = envelope.original_sender_id.unwrap_or(envelope.sender_id);
        let search_key = envelope
            .payload
            .search_id
            .map(|search_id| (sender_id, search_id));
        let log_source = log_source_from_proto(
            envelope
                .payload
                .log_source
                .context("missing search commit log source")?,
        )?;

        let (request_tx, request_rx) = async_channel::unbounded();
        let search_task = repository_handle.update(&mut cx, |repository, cx| {
            repository.search_commits(log_source, search_args, request_tx, cx)
        });

        let (mut response_tx, response_rx) = mpsc::unbounded();
        let forward_task = cx.background_spawn(async move {
            // Said before any result, and even when there are none, so that the client can tell
            // this server applied every filter from an older one that dropped some.
            if response_tx
                .send(Ok(proto::SearchCommitsResponse {
                    shas: Vec::new(),
                    filters_version: Some(SEARCH_COMMITS_FILTERS_VERSION),
                    cancellable: Some(true),
                }))
                .await
                .is_err()
            {
                return;
            }

            let mut shas = Vec::new();
            let mut found_count = 0;
            let mut reached_limit = false;

            while let Ok(sha) = request_rx.recv().await {
                shas.push(sha.to_string());
                found_count += 1;
                if found_count >= COMMIT_SEARCH_RESULT_LIMIT {
                    reached_limit = true;
                    break;
                }

                if shas.len() >= CHUNK_SIZE {
                    if response_tx
                        .send(Ok(proto::SearchCommitsResponse {
                            shas: mem::take(&mut shas),
                            filters_version: None,
                            cancellable: None,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }

            if !shas.is_empty() {
                response_tx
                    .send(Ok(proto::SearchCommitsResponse {
                        shas,
                        filters_version: None,
                        cancellable: None,
                    }))
                    .await
                    .ok();
            }

            // A client too old to cancel stops listening at the same limit without saying so; the
            // search is dropped here, which stops git, instead of walking the rest of the history.
            if reached_limit {
                drop(search_task);
                return;
            }

            if let Err(error) = search_task.await {
                response_tx
                    .send(Err(anyhow!("{error:#}")))
                    .await
                    .context("failed to send the commit search error")
                    .log_err();
            }
        });

        match search_key {
            None => forward_task.detach(),
            Some(search_key) => {
                let (cancel_tx, cancel_rx) = oneshot::channel();
                this.update(&mut cx, |this, _| {
                    this.commit_searches.insert(search_key, cancel_tx)
                });
                let this = this.downgrade();
                // Winning the race drops the forwarding task, which drops the search (stopping
                // git) and ends the response stream.
                cx.spawn(async move |cx| {
                    future::select(forward_task, cancel_rx).await;
                    this.update(cx, |this, _| this.commit_searches.remove(&search_key))
                        .ok();
                })
                .detach();
            }
        }

        Ok(response_rx)
    }

    async fn handle_cancel_search_commits(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CancelSearchCommits>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let search_key = (
            envelope.original_sender_id.unwrap_or(envelope.sender_id),
            envelope.payload.search_id,
        );
        this.update(&mut cx, |this, _| {
            if let Some(cancel_tx) = this.commit_searches.remove(&search_key) {
                cancel_tx.send(()).ok();
            }
        });
        Ok(())
    }

    async fn handle_edit_ref(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitEditRef>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let ref_name = envelope.payload.ref_name;
        let commit = match envelope.payload.action {
            Some(proto::git_edit_ref::Action::UpdateToCommit(sha)) => Some(sha),
            Some(proto::git_edit_ref::Action::Delete(_)) => None,
            None => anyhow::bail!("GitEditRef missing action"),
        };

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.edit_ref(ref_name, commit)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_repair_worktrees(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRepairWorktrees>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.repair_worktrees()
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_get_branches(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitGetBranches>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitBranchesResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branches_scan = repository_handle
            .update(&mut cx, |repository_handle, _| repository_handle.branches())
            .await??;

        Ok(proto::GitBranchesResponse {
            branches: branches_scan
                .branches
                .into_iter()
                .map(|branch| branch_to_proto(&branch))
                .collect::<Vec<_>>(),
            error: branches_scan.error.map(|error| error.to_string()),
        })
    }
    async fn handle_get_default_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetDefaultBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetDefaultBranchResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branch = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.default_branch(envelope.payload.include_remote_name)
            })
            .await??
            .map(Into::into);

        Ok(proto::GetDefaultBranchResponse { branch })
    }
    async fn handle_create_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;
        let base_branch = envelope.payload.base_branch;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.create_branch(branch_name, base_branch)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_change_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitChangeBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let branch_name = envelope.payload.branch_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.change_branch(branch_name)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_rename_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRenameBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let branch = envelope.payload.branch;
        let new_name = envelope.payload.new_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.rename_branch(branch, new_name)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_create_remote(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateRemote>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let remote_name = envelope.payload.remote_name;
        let remote_url = envelope.payload.remote_url;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.create_remote(remote_name, remote_url)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_delete_branch(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitDeleteBranch>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let is_remote = envelope.payload.is_remote;
        let branch_name = envelope.payload.branch_name;
        let force = envelope.payload.force;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.delete_branch(is_remote, branch_name, force)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_remove_remote(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRemoveRemote>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let remote_name = envelope.payload.remote_name;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.remove_remote(remote_name)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_show(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitShow>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCommitDetails> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let commit = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.show(envelope.payload.commit)
            })
            .await??;
        Ok(proto::GitCommitDetails {
            sha: commit.sha.into(),
            message: commit.message.into(),
            commit_timestamp: commit.commit_timestamp,
            author_email: commit.author_email.into(),
            author_name: commit.author_name.into(),
        })
    }

    async fn handle_create_checkpoint(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateCheckpoint>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCreateCheckpointResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let checkpoint = repository_handle
            .update(&mut cx, |repository, _| repository.checkpoint())
            .await??;

        Ok(proto::GitCreateCheckpointResponse {
            commit_sha: checkpoint.commit_sha.as_bytes().to_vec(),
        })
    }

    async fn handle_create_archive_checkpoint(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCreateArchiveCheckpoint>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCreateArchiveCheckpointResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let (staged_commit_sha, unstaged_commit_sha) = repository_handle
            .update(&mut cx, |repository, _| {
                repository.create_archive_checkpoint()
            })
            .await??;

        Ok(proto::GitCreateArchiveCheckpointResponse {
            staged_commit_sha,
            unstaged_commit_sha,
        })
    }

    async fn handle_restore_checkpoint(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRestoreCheckpoint>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let checkpoint = GitRepositoryCheckpoint {
            commit_sha: Oid::from_bytes(&envelope.payload.commit_sha)?,
        };

        repository_handle
            .update(&mut cx, |repository, _| {
                repository.restore_checkpoint(checkpoint)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_restore_archive_checkpoint(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRestoreArchiveCheckpoint>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let staged_commit_sha = envelope.payload.staged_commit_sha;
        let unstaged_commit_sha = envelope.payload.unstaged_commit_sha;

        repository_handle
            .update(&mut cx, |repository, _| {
                repository.restore_archive_checkpoint(staged_commit_sha, unstaged_commit_sha)
            })
            .await??;

        Ok(proto::Ack {})
    }

    async fn handle_compare_checkpoints(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCompareCheckpoints>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitCompareCheckpointsResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let left = GitRepositoryCheckpoint {
            commit_sha: Oid::from_bytes(&envelope.payload.left_commit_sha)?,
        };
        let right = GitRepositoryCheckpoint {
            commit_sha: Oid::from_bytes(&envelope.payload.right_commit_sha)?,
        };

        let equal = repository_handle
            .update(&mut cx, |repository, _| {
                repository.compare_checkpoints(left, right)
            })
            .await??;

        Ok(proto::GitCompareCheckpointsResponse { equal })
    }

    async fn handle_diff_checkpoints(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitDiffCheckpoints>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitDiffCheckpointsResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let base = GitRepositoryCheckpoint {
            commit_sha: Oid::from_bytes(&envelope.payload.base_commit_sha)?,
        };
        let target = GitRepositoryCheckpoint {
            commit_sha: Oid::from_bytes(&envelope.payload.target_commit_sha)?,
        };

        let diff = repository_handle
            .update(&mut cx, |repository, _| {
                repository.diff_checkpoints(base, target)
            })
            .await??;

        Ok(proto::GitDiffCheckpointsResponse { diff })
    }

    async fn handle_load_commit_diff(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::LoadCommitDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::LoadCommitDiffResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let commit_diff = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.load_commit_diff(
                    envelope.payload.commit,
                    envelope.payload.ignore_shallow_boundary,
                )
            })
            .await??;
        Ok(proto::LoadCommitDiffResponse {
            files: commit_diff
                .files
                .into_iter()
                .map(|file| proto::CommitFile {
                    path: file.path.as_unix_str().to_owned(),
                    old_text: file.old_text,
                    new_text: file.new_text,
                    is_binary: file.is_binary,
                })
                .collect(),
            is_shallow_boundary: commit_diff.is_shallow_boundary,
        })
    }

    async fn handle_reset(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitReset>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let mode = match envelope.payload.mode() {
            git_reset::ResetMode::Soft => ResetMode::Soft,
            git_reset::ResetMode::Mixed => ResetMode::Mixed,
            git_reset::ResetMode::Hard => ResetMode::Hard,
        };

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.reset(envelope.payload.commit, mode, cx)
            })
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_checkout_files(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitCheckoutFiles>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let paths = envelope
            .payload
            .paths
            .iter()
            .map(|s| RepoPath::from_proto(s))
            .collect::<Result<Vec<_>>>()?;

        repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.checkout_files(&envelope.payload.commit, paths, cx)
            })
            .await?;
        Ok(proto::Ack {})
    }

    async fn handle_add_path_to_gitignore(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitAddPathToGitignore>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let repo_path = RepoPath::from_proto(&envelope.payload.path)?;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.add_path_to_gitignore(&repo_path, envelope.payload.is_dir)
            })
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_add_path_to_git_info_exclude(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitAddPathToGitInfoExclude>,
        mut cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let repo_path = RepoPath::from_proto(&envelope.payload.path)?;

        repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.add_path_to_git_info_exclude(&repo_path, envelope.payload.is_dir)
            })
            .await??;
        Ok(proto::Ack {})
    }

    async fn handle_open_commit_message_buffer(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::OpenCommitMessageBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenBufferResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let buffer = repository
            .update(&mut cx, |repository, cx| {
                repository.open_commit_buffer(None, this.read(cx).buffer_store.clone(), cx)
            })
            .await?;

        let buffer_id = buffer.read_with(&cx, |buffer, _| buffer.remote_id());
        this.update(&mut cx, |this, cx| {
            this.buffer_store.update(cx, |buffer_store, cx| {
                buffer_store
                    .create_buffer_for_peer(
                        &buffer,
                        envelope.original_sender_id.unwrap_or(envelope.sender_id),
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
        });

        Ok(proto::OpenBufferResponse {
            buffer_id: buffer_id.to_proto(),
        })
    }

    async fn handle_askpass(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::AskPassRequest>,
        mut cx: AsyncApp,
    ) -> Result<proto::AskPassResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let delegates = cx.update(|cx| repository.read(cx).askpass_delegates.clone());
        let response = request_remote_password(
            &delegates,
            envelope.payload.askpass_id,
            envelope.payload.prompt,
        )
        .await?;

        // In fact, we don't quite know what we're doing here, as we're sending askpass password unencrypted, but..
        Ok(proto::AskPassResponse {
            response: response.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)?,
        })
    }

    async fn handle_check_for_pushed_commits(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::CheckForPushedCommits>,
        mut cx: AsyncApp,
    ) -> Result<proto::CheckForPushedCommitsResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;

        let branches = repository_handle
            .update(&mut cx, |repository_handle, _| {
                repository_handle.check_for_pushed_commits()
            })
            .await??;
        Ok(proto::CheckForPushedCommitsResponse {
            pushed_to: branches
                .into_iter()
                .map(|commit| commit.to_string())
                .collect(),
        })
    }

    async fn handle_git_diff(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitDiffResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let diff_type = match envelope.payload.diff_type() {
            proto::git_diff::DiffType::HeadToIndex => DiffType::HeadToIndex,
            proto::git_diff::DiffType::HeadToWorktree => DiffType::HeadToWorktree,
            proto::git_diff::DiffType::MergeBase => {
                let base_ref = envelope
                    .payload
                    .merge_base_ref
                    .ok_or_else(|| anyhow!("merge_base_ref is required for MergeBase diff type"))?;
                DiffType::MergeBase {
                    base_ref: base_ref.into(),
                }
            }
        };

        let mut diff = repository_handle
            .update(&mut cx, |repository_handle, cx| {
                repository_handle.diff(diff_type, cx)
            })
            .await??;
        const ONE_MB: usize = 1_000_000;
        if diff.len() > ONE_MB {
            diff = diff.chars().take(ONE_MB).collect()
        }

        Ok(proto::GitDiffResponse { diff })
    }

    async fn handle_run_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GitRunCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::GitRunCommandResponse> {
        let repository_id = RepositoryId::from_proto(envelope.payload.repository_id);
        let repository_handle = Self::repository_for_request(&this, repository_id, &mut cx)?;
        let args = envelope.payload.args;
        let stdin = envelope.payload.stdin;
        let mut command_env: HashMap<String, String> = envelope.payload.env.into_iter().collect();
        let without_hooks = command_env
            .remove(GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY)
            .is_some();

        let output = repository_handle
            .update(&mut cx, |repository_handle, _| {
                if without_hooks {
                    repository_handle.run_git_command_without_hooks(args, stdin, command_env)
                } else {
                    repository_handle.run_git_command_with_env(args, stdin, command_env)
                }
            })
            .await??;

        Ok(proto::GitRunCommandResponse {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            hooks_disabled: without_hooks,
        })
    }

    async fn handle_tree_diff(
        this: Entity<Self>,
        request: TypedEnvelope<proto::GetTreeDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetTreeDiffResponse> {
        let repository_id = RepositoryId(request.payload.repository_id);
        let diff_type = if request.payload.includes_worktree {
            DiffTreeType::MergeBaseWithWorktree {
                base: request.payload.base.into(),
            }
        } else if request.payload.is_merge {
            DiffTreeType::MergeBase {
                base: request.payload.base.into(),
                head: request.payload.head.into(),
            }
        } else {
            DiffTreeType::Since {
                base: request.payload.base.into(),
                head: request.payload.head.into(),
            }
        };

        let diff = this
            .update(&mut cx, |this, cx| {
                let repository = this.repositories().get(&repository_id)?;
                Some(repository.update(cx, |repo, cx| repo.diff_tree(diff_type, cx)))
            })
            .context("missing repository")?
            .await??;

        Ok(proto::GetTreeDiffResponse {
            entries: diff
                .entries
                .into_iter()
                .map(|(path, status)| proto::TreeDiffStatus {
                    path: path.as_ref().as_unix_str().to_owned(),
                    status: match status {
                        TreeDiffStatus::Added {} => proto::tree_diff_status::Status::Added.into(),
                        TreeDiffStatus::Modified { .. } => {
                            proto::tree_diff_status::Status::Modified.into()
                        }
                        TreeDiffStatus::Deleted { .. } => {
                            proto::tree_diff_status::Status::Deleted.into()
                        }
                    },
                    oid: match status {
                        TreeDiffStatus::Deleted { old } | TreeDiffStatus::Modified { old } => {
                            Some(old.to_string())
                        }
                        TreeDiffStatus::Added => None,
                    },
                })
                .collect(),
        })
    }

    async fn handle_get_blob_content(
        this: Entity<Self>,
        request: TypedEnvelope<proto::GetBlobContent>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetBlobContentResponse> {
        let oid = git::Oid::from_str(&request.payload.oid)?;
        let repository_id = RepositoryId(request.payload.repository_id);
        let content = this
            .update(&mut cx, |this, cx| {
                let repository = this.repositories().get(&repository_id)?;
                Some(repository.update(cx, |repo, cx| repo.load_blob_content(oid, cx)))
            })
            .context("missing repository")?
            .await?;
        Ok(proto::GetBlobContentResponse { content })
    }

    async fn handle_load_commit_template(
        this: Entity<Self>,
        request: TypedEnvelope<proto::LoadCommitTemplate>,
        mut cx: AsyncApp,
    ) -> Result<proto::LoadCommitTemplateResponse> {
        let repository_id = RepositoryId(request.payload.repository_id);
        let rx = this
            .update(&mut cx, |this, cx| {
                let repository = this.repositories().get(&repository_id)?;
                Some(repository.update(cx, |repo, _| repo.load_commit_template_text()))
            })
            .context("missing repository")?;
        let template = rx.await??;
        Ok(proto::LoadCommitTemplateResponse {
            template: template.map(|t| t.template),
        })
    }

    async fn handle_open_unstaged_diff(
        this: Entity<Self>,
        request: TypedEnvelope<proto::OpenUnstagedDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenUnstagedDiffResponse> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        let diff = this
            .update(&mut cx, |this, cx| {
                let buffer = this.buffer_store.read(cx).get(buffer_id)?;
                Some(this.open_unstaged_diff(buffer, cx))
            })
            .context("missing buffer")?
            .await?;
        this.update(&mut cx, |this, _| {
            let shared_diffs = this
                .shared_diffs
                .entry(request.original_sender_id.unwrap_or(request.sender_id))
                .or_default();
            shared_diffs.entry(buffer_id).or_default().unstaged = Some(diff.clone());
        });
        let staged_text = diff.read_with(&cx, |diff, cx| diff.base_text_string(cx));
        Ok(proto::OpenUnstagedDiffResponse { staged_text })
    }

    async fn handle_open_uncommitted_diff(
        this: Entity<Self>,
        request: TypedEnvelope<proto::OpenUncommittedDiff>,
        mut cx: AsyncApp,
    ) -> Result<proto::OpenUncommittedDiffResponse> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        let diff = this
            .update(&mut cx, |this, cx| {
                let buffer = this.buffer_store.read(cx).get(buffer_id)?;
                Some(this.open_uncommitted_diff(buffer, cx))
            })
            .context("missing buffer")?
            .await?;
        this.update(&mut cx, |this, _| {
            let shared_diffs = this
                .shared_diffs
                .entry(request.original_sender_id.unwrap_or(request.sender_id))
                .or_default();
            shared_diffs.entry(buffer_id).or_default().uncommitted = Some(diff.clone());
        });
        this.read_with(&cx, |this, cx| {
            use proto::open_uncommitted_diff_response::Mode;

            let diff_state = this.diffs.get(&buffer_id).context("missing diff state")?;
            let diff_state = diff_state.read(cx);
            let index_matches_head = diff_state.index_matches_head();
            let index_text = diff_state.index_text.clone();
            let head_text = diff_state.head_text.clone();

            let response = if index_matches_head {
                proto::OpenUncommittedDiffResponse {
                    committed_text: head_text.map(|head| head.to_string()),
                    staged_text: None,
                    mode: Mode::IndexMatchesHead.into(),
                }
            } else {
                proto::OpenUncommittedDiffResponse {
                    committed_text: head_text.map(|head| head.to_string()),
                    staged_text: index_text.map(|index| index.to_string()),
                    mode: Mode::IndexAndHead.into(),
                }
            };
            anyhow::Ok(response)
        })
    }

    async fn handle_update_diff_bases(
        this: Entity<Self>,
        request: TypedEnvelope<proto::UpdateDiffBases>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        let buffer_id = BufferId::new(request.payload.buffer_id)?;
        this.update(&mut cx, |this, cx| {
            if let Some(diff_state) = this.diffs.get_mut(&buffer_id)
                && let Some(buffer) = this.buffer_store.read(cx).get(buffer_id)
            {
                let buffer = buffer.read(cx).text_snapshot();
                diff_state.update(cx, |diff_state, cx| {
                    diff_state.handle_base_texts_updated(buffer, request.payload, cx);
                })
            }
        });
        Ok(())
    }

    async fn handle_blame_buffer(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::BlameBuffer>,
        mut cx: AsyncApp,
    ) -> Result<proto::BlameBufferResponse> {
        let buffer_id = BufferId::new(envelope.payload.buffer_id)?;
        let version = deserialize_version(&envelope.payload.version);
        let buffer = this.read_with(&cx, |this, cx| {
            this.buffer_store.read(cx).get_existing(buffer_id)
        })?;
        buffer
            .update(&mut cx, |buffer, _| {
                buffer.wait_for_version(version.clone())
            })
            .await?;
        let blame = this
            .update(&mut cx, |this, cx| {
                this.blame_buffer(&buffer, Some(version), cx)
            })
            .await?;
        Ok(serialize_blame_buffer_response(blame))
    }

    async fn handle_blame_buffer_at_revision(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::BlameBufferAtRevision>,
        mut cx: AsyncApp,
    ) -> Result<proto::BlameBufferAtRevisionResponse> {
        let repository_id = RepositoryId(envelope.payload.repository_id);
        let path = RepoPath::from_proto(&envelope.payload.path)?;
        let revision = envelope
            .payload
            .revision
            .parse::<git::Oid>()
            .with_context(|| format!("invalid revision {:?}", envelope.payload.revision))?;
        let (content, blame) = this
            .update(&mut cx, |this, cx| {
                let repository = this.repositories().get(&repository_id)?;
                Some(repository.update(cx, |repository, cx| {
                    repository.blame_buffer_at_revision(path, revision, cx)
                }))
            })
            .context("missing repository")?
            .await?;
        Ok(proto::BlameBufferAtRevisionResponse {
            content,
            entries: blame
                .entries
                .into_iter()
                .map(serialize_blame_entry)
                .collect(),
            messages: serialize_commit_messages(blame.messages),
            tag_names: serialize_commit_tag_names(blame.tag_names),
        })
    }

    async fn handle_get_permalink_to_line(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetPermalinkToLine>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetPermalinkToLineResponse> {
        let buffer_id = BufferId::new(envelope.payload.buffer_id)?;
        let selection = {
            let selection = envelope
                .payload
                .selection
                .context("no selection to get permalink for defined")?;
            selection.start as u32..selection.end as u32
        };
        let buffer = this.read_with(&cx, |this, cx| {
            this.buffer_store.read(cx).get_existing(buffer_id)
        })?;
        let permalink = this
            .update(&mut cx, |this, cx| {
                this.get_permalink_to_line(&buffer, selection, cx)
            })
            .await?;

        Ok(proto::GetPermalinkToLineResponse {
            permalink: permalink.to_string(),
        })
    }

    async fn handle_get_file_permalink(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetFilePermalink>,
        mut cx: AsyncApp,
    ) -> Result<proto::GetFilePermalinkResponse> {
        let path = envelope
            .payload
            .path
            .context("GetFilePermalink requires a path")?;
        let path = ProjectPath::from_proto(path).context("invalid file permalink path")?;
        let permalink = this
            .update(&mut cx, |this, cx| this.get_file_permalink(&path, cx))
            .await?;

        Ok(proto::GetFilePermalinkResponse {
            permalink: permalink.to_string(),
        })
    }

    fn repository_for_request(
        this: &Entity<Self>,
        id: RepositoryId,
        cx: &mut AsyncApp,
    ) -> Result<Entity<Repository>> {
        this.read_with(cx, |this, _| {
            this.repositories
                .get(&id)
                .context("missing repository handle")
                .cloned()
        })
    }

    pub fn repo_snapshots(&self, cx: &App) -> HashMap<RepositoryId, RepositorySnapshot> {
        self.repositories
            .iter()
            .map(|(id, repo)| (*id, repo.read(cx).snapshot.clone()))
            .collect()
    }

    pub fn display_repo_snapshots(&self, cx: &App) -> HashMap<RepositoryId, RepositorySnapshot> {
        let mut snapshots = self.repo_snapshots(cx);
        if self.diff_base == GitDiffBaseSetting::Head {
            return snapshots;
        }
        for (id, snapshot) in &mut snapshots {
            if let Some(statuses) = self
                .display_diffs
                .get(id)
                .and_then(|state| state.buffers.as_ref())
                .and_then(|(buffers, _)| buffers.read(cx).statuses_by_path())
            {
                snapshot.statuses_by_path = statuses;
            }
        }
        snapshots
    }

    pub fn diff_base(&self) -> GitDiffBaseSetting {
        self.diff_base
    }

    fn set_diff_base(&mut self, setting: GitDiffBaseSetting, cx: &mut Context<Self>) {
        if self.diff_base == setting {
            return;
        }
        self.diff_base = setting;
        self.display_diffs.clear();
        if setting == GitDiffBaseSetting::DefaultBranch {
            let repo_ids = self.repositories.keys().copied().collect::<Vec<_>>();
            for repo_id in repo_ids {
                self.refresh_diff_base_for_repo(repo_id, cx);
            }
        }
        cx.emit(GitStoreEvent::DiffBaseChanged(None));
    }

    pub fn display_status_for_buffer_id(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<FileStatus> {
        let (repo, repo_path) = self.repository_and_path_for_buffer_id(buffer_id, cx)?;
        self.display_status(&repo, &repo_path, cx)
    }

    pub fn display_status_for_project_path(
        &self,
        project_path: &ProjectPath,
        cx: &App,
    ) -> Option<FileStatus> {
        let (repo, repo_path) = self.repository_and_path_for_project_path(project_path, cx)?;
        self.display_status(&repo, &repo_path, cx)
    }

    fn display_status(
        &self,
        repo: &Entity<Repository>,
        repo_path: &RepoPath,
        cx: &App,
    ) -> Option<FileStatus> {
        if self.diff_base == GitDiffBaseSetting::DefaultBranch
            && let Some((buffers, _)) = self
                .display_diffs
                .get(&repo.read(cx).id)
                .and_then(|state| state.buffers.as_ref())
        {
            return buffers.read(cx).status_for_path(repo_path, cx);
        }
        repo.read(cx)
            .status_for_path(repo_path)
            .map(|entry| entry.status)
    }

    pub fn display_diff_for_repo(
        &self,
        repo_id: RepositoryId,
    ) -> Option<Entity<diff_buffer_list::DiffBufferList>> {
        let (buffers, _) = self.display_diffs.get(&repo_id)?.buffers.as_ref()?;
        Some(buffers.clone())
    }

    pub fn ensure_display_diff(
        &mut self,
        repo: Entity<Repository>,
        resolved_ref: SharedString,
        cx: &mut Context<Self>,
    ) -> Entity<diff_buffer_list::DiffBufferList> {
        let repo_id = repo.read(cx).id;
        if let Some((buffers, _)) = self
            .display_diffs
            .get(&repo_id)
            .and_then(|state| state.buffers.as_ref())
        {
            let buffers = buffers.clone();
            buffers.update(cx, |buffers, cx| {
                buffers.set_diff_base(
                    diff_buffer_list::DiffBase::Merge {
                        base_ref: resolved_ref,
                    },
                    cx,
                )
            });
            return buffers;
        }

        let git_store = cx.entity();
        let buffers = cx.new(|cx| {
            diff_buffer_list::DiffBufferList::new(
                diff_buffer_list::DiffBase::Merge {
                    base_ref: resolved_ref,
                },
                git_store,
                Some(repo),
                cx,
            )
        });
        let subscription = cx.subscribe(&buffers, move |_, _, event, cx| {
            if matches!(event, diff_buffer_list::BranchDiffEvent::FileListChanged) {
                cx.emit(GitStoreEvent::DiffBaseChanged(Some(repo_id)));
            }
        });
        self.display_diffs.entry(repo_id).or_default().buffers =
            Some((buffers.clone(), subscription));
        buffers
    }

    fn refresh_diff_base_for_repo(&mut self, repo_id: RepositoryId, cx: &mut Context<Self>) {
        if self.diff_base == GitDiffBaseSetting::Head {
            return;
        }
        let Some(repo) = self.repositories.get(&repo_id).cloned() else {
            return;
        };

        let task = cx.spawn(async move |this, cx| {
            let result: Result<Option<SharedString>> =
                async { repo.update(cx, |repo, _| repo.default_branch(true)).await? }.await;
            this.update(cx, |this, cx| {
                let Some(state) = this.display_diffs.get_mut(&repo_id) else {
                    return;
                };
                state.refresh.take();
                match result.as_ref() {
                    Ok(Some(resolved_ref)) => {
                        this.ensure_display_diff(repo, resolved_ref.clone(), cx);
                        cx.emit(GitStoreEvent::DiffBaseChanged(Some(repo_id)));
                    }
                    Ok(None) => {
                        if state.buffers.take().is_some() {
                            cx.emit(GitStoreEvent::DiffBaseChanged(Some(repo_id)));
                        }
                    }
                    Err(_) => {}
                }
            })
            .log_err();
            result.log_err();
        });
        self.display_diffs.entry(repo_id).or_default().refresh = Some(task);
    }

    fn coalesce_repo_paths(mut paths: Vec<RepoPath>) -> Vec<RepoPath> {
        paths.sort();

        let mut coalesced = Vec::with_capacity(paths.len());
        for path in paths {
            if coalesced
                .last()
                .is_some_and(|ancestor: &RepoPath| path.starts_with(ancestor))
            {
                continue;
            }
            coalesced.push(path);
        }

        coalesced
    }

    fn process_updated_entries(
        &self,
        worktree: &Entity<Worktree>,
        updated_entries: &[(Arc<RelPath>, ProjectEntryId, PathChange)],
        cx: &mut App,
    ) -> Task<HashMap<Entity<Repository>, Vec<RepoPath>>> {
        let path_style = worktree.read(cx).path_style();
        let mut repo_paths = self
            .repositories
            .values()
            .map(|repo| (repo.read(cx).work_directory_abs_path.clone(), repo.clone()))
            .collect::<Vec<_>>();
        let mut entries: Vec<_> = updated_entries
            .iter()
            .map(|(path, _, _)| path.clone())
            .collect();
        entries.sort();
        let worktree = worktree.read(cx);

        let entries = entries
            .into_iter()
            .map(|path| worktree.absolutize(&path))
            .collect::<Arc<[_]>>();

        let executor = cx.background_executor().clone();
        cx.background_executor().spawn(async move {
            repo_paths.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));
            let mut paths_by_git_repo = HashMap::<_, Vec<_>>::default();
            let mut tasks = FuturesOrdered::new();
            for (repo_path, repo) in repo_paths.into_iter().rev() {
                let entries = entries.clone();
                let task = executor.spawn(async move {
                    // Find all repository paths that belong to this repo
                    let mut ix = entries.partition_point(|path| path < &*repo_path);
                    if ix == entries.len() {
                        return None;
                    };

                    let mut paths = Vec::new();
                    // All paths prefixed by a given repo will constitute a continuous range.
                    while let Some(path) = entries.get(ix)
                        && let Some(repo_path) = RepositorySnapshot::abs_path_to_repo_path_inner(
                            &repo_path, path, path_style,
                        )
                    {
                        paths.push((repo_path, ix));
                        ix += 1;
                    }
                    if paths.is_empty() {
                        None
                    } else {
                        Some((repo, paths))
                    }
                });
                tasks.push_back(task);
            }

            // Now, let's filter out the "duplicate" entries that were processed by multiple distinct repos.
            let mut path_was_used = vec![false; entries.len()];
            let tasks = tasks.collect::<Vec<_>>().await;
            // Process tasks from the back: iterating backwards allows us to see more-specific paths first.
            // We always want to assign a path to it's innermost repository.
            for t in tasks {
                let Some((repo, paths)) = t else {
                    continue;
                };
                let entry = paths_by_git_repo.entry(repo).or_default();
                for (repo_path, ix) in paths {
                    if path_was_used[ix] {
                        continue;
                    }
                    path_was_used[ix] = true;
                    entry.push(repo_path);
                }
            }

            for paths in paths_by_git_repo.values_mut() {
                *paths = Self::coalesce_repo_paths(mem::take(paths));
            }

            paths_by_git_repo
        })
    }
}

impl BufferGitState {
    fn new(_git_store: WeakEntity<GitStore>, _cx: &mut Context<Self>) -> Self {
        Self {
            unstaged_diff: Default::default(),
            staged_diff: Default::default(),
            uncommitted_diff: Default::default(),
            oid_diffs: Default::default(),
            recalculate_diff_task: Default::default(),
            language: Default::default(),
            language_registry: Default::default(),
            recalculating_tx: postage::watch::channel_with(false).0,
            hunk_staging_operation_count: 0,
            hunk_staging_operation_count_as_of_write: 0,
            head_text: Default::default(),
            index_text: Default::default(),
            pending_index_edits: Some(Vec::new()),
            oid_texts: Default::default(),
            head_text_buffer: WeakEntity::new_invalid(),
            index_text_buffer: WeakEntity::new_invalid(),
            index_text_buffer_language_enabled: Default::default(),
            head_changed: Default::default(),
            index_changed: Default::default(),
            language_changed: Default::default(),
            conflict_updated_futures: Default::default(),
            conflict_set: Default::default(),
            reparse_conflict_markers_task: Default::default(),
        }
    }

    fn get_or_create_head_text_buffer(&mut self, cx: &mut Context<Self>) -> Entity<Buffer> {
        if let Some(buffer) = self.head_text_buffer.upgrade() {
            return buffer;
        }
        let head_text = self.head_text.clone();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(head_text.as_deref().unwrap_or(""), cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        self.head_text_buffer = buffer.downgrade();
        buffer
    }

    fn index_text_buffer(&self) -> Option<Entity<Buffer>> {
        self.index_text_buffer.upgrade()
    }

    fn get_or_create_index_text_buffer(
        &mut self,
        file: Option<Arc<dyn language::File>>,
        cx: &mut Context<Self>,
    ) -> Entity<Buffer> {
        if let Some(buffer) = self.index_text_buffer.upgrade() {
            if let Some(file) = file {
                buffer.update(cx, |buffer, cx| buffer.file_updated(file, cx));
            }
            return buffer;
        }
        let index_text = self.index_text.clone();
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(index_text.as_deref().unwrap_or(""), cx);
            if let Some(file) = file {
                buffer.file_updated(file, cx);
            }
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        self.index_text_buffer = buffer.downgrade();
        buffer
    }

    #[ztracing::instrument(skip_all)]
    fn buffer_language_changed(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.language = buffer.read(cx).language().cloned();
        self.language_changed = true;
        let _ = self.recalculate_diffs(buffer.read(cx).text_snapshot(), cx);
    }

    fn reparse_conflict_markers(
        &mut self,
        buffer: text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();

        let Some(conflict_set) = self
            .conflict_set
            .as_ref()
            .and_then(|conflict_set| conflict_set.upgrade())
        else {
            return rx;
        };

        let has_conflict = conflict_set.read_with(cx, |conflict_set, _| conflict_set.has_conflict);
        if !has_conflict {
            return rx;
        }

        let old_snapshot = conflict_set.read_with(cx, |conflict_set, _| conflict_set.snapshot());
        self.conflict_updated_futures.push(tx);
        self.reparse_conflict_markers_task = Some(cx.spawn(async move |this, cx| {
            let (snapshot, changed_range) = cx
                .background_spawn(async move {
                    let new_snapshot = ConflictSet::parse(&buffer);
                    let changed_range = old_snapshot.compare(&new_snapshot, &buffer);
                    (new_snapshot, changed_range)
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(conflict_set) = &this.conflict_set {
                    conflict_set
                        .update(cx, |conflict_set, cx| {
                            conflict_set.set_snapshot(snapshot, changed_range, cx);
                        })
                        .ok();
                }
                let futures = std::mem::take(&mut this.conflict_updated_futures);
                for tx in futures {
                    tx.send(()).ok();
                }
            })
        }));

        rx
    }

    fn unstaged_diff(&self) -> Option<Entity<BufferDiff>> {
        self.unstaged_diff.as_ref().and_then(|set| set.upgrade())
    }

    fn staged_diff(&self) -> Option<Entity<BufferDiff>> {
        self.staged_diff.as_ref().and_then(|(set, _)| set.upgrade())
    }

    fn staged_diff_and_index_text_buffer(&self) -> Option<(Entity<BufferDiff>, Entity<Buffer>)> {
        let (diff, index_text_buffer) = self.staged_diff.as_ref()?;
        Some((diff.upgrade()?, index_text_buffer.clone()))
    }

    fn uncommitted_diff(&self) -> Option<Entity<BufferDiff>> {
        self.uncommitted_diff.as_ref().and_then(|set| set.upgrade())
    }

    fn oid_diff(&self, oid: Option<git::Oid>) -> Option<Entity<BufferDiff>> {
        self.oid_diffs.get(&oid).and_then(|weak| weak.upgrade())
    }

    /// Whether the index text is known to match the committed text, without
    /// comparing their contents. Always true when both texts were set by a
    /// single `DiffBasesChange::SetBoth`, which shares one allocation between
    /// them. May be false even when the contents are equal, if the texts were
    /// loaded separately.
    fn index_matches_head(&self) -> bool {
        match (self.index_text.as_ref(), self.head_text.as_ref()) {
            (Some(index), Some(head)) => Arc::ptr_eq(index, head),
            (None, None) => true,
            _ => false,
        }
    }

    fn remove_overlapping_pending_index_edits(&mut self, ranges: &[Range<usize>]) {
        if let Some(edits) = &mut self.pending_index_edits {
            edits.retain(|(existing, _)| {
                ranges.iter().all(|footprint| {
                    existing.end < footprint.start || footprint.end < existing.start
                })
            });
        }
    }

    fn insert_pending_index_edits(&mut self, edits: Option<Vec<(Range<usize>, Arc<str>)>>) {
        match edits {
            None => {
                self.pending_index_edits = None;
            }
            Some(new_edits) => {
                let mut edits = self.pending_index_edits.take().unwrap_or_default();
                for (range, replacement) in new_edits {
                    edits.retain(|(existing, _)| {
                        existing.end < range.start || range.end < existing.start
                    });
                    let position =
                        edits.partition_point(|(existing, _)| existing.start < range.start);
                    edits.insert(position, (range, replacement));
                }
                self.pending_index_edits = Some(edits);
            }
        }
    }

    fn pending_index_text(&self, cx: &App) -> Option<Rope> {
        let index_text_buffer = self.index_text_buffer.upgrade()?;
        let edits = self.pending_index_edits.as_ref()?;
        #[cfg(debug_assertions)]
        for window in edits.windows(2) {
            debug_assert!(window[0].0.end <= window[1].0.start);
        }
        let mut index_text = index_text_buffer.read(cx).text_snapshot().as_rope().clone();
        for (old_range, replacement_text) in edits.iter().rev() {
            index_text.replace(old_range.clone(), replacement_text);
        }
        Some(index_text)
    }

    fn clear_pending_index_edits(&mut self) {
        self.pending_index_edits = Some(Vec::new());
    }

    fn clear_pending_hunks(&mut self, cx: &mut Context<Self>) {
        for diff in [
            self.uncommitted_diff(),
            self.unstaged_diff(),
            self.staged_diff(),
        ]
        .into_iter()
        .flatten()
        {
            diff.update(cx, |diff, cx| diff.clear_pending_hunks(cx));
        }
    }

    fn mark_whole_file_stage_or_unstage_pending(
        &mut self,
        stage: bool,
        buffer_snapshot: &text::BufferSnapshot,
        cx: &mut Context<Self>,
    ) {
        if let Some(uncommitted_diff) = self.uncommitted_diff() {
            uncommitted_diff.update(cx, |uncommitted_diff, cx| {
                uncommitted_diff.mark_all_hunks_pending(stage, buffer_snapshot, cx);
            });
        }

        if stage {
            if let Some(unstaged_diff) = self.unstaged_diff() {
                unstaged_diff.update(cx, |unstaged_diff, cx| {
                    unstaged_diff.suppress_all_hunks_pending(buffer_snapshot, cx);
                });
            }
        } else if let Some(staged_diff) = self.staged_diff()
            && let Some(index_text_buffer) = self.index_text_buffer()
        {
            let index_snapshot = index_text_buffer.read(cx).text_snapshot();
            staged_diff.update(cx, |staged_diff, cx| {
                staged_diff.suppress_all_hunks_pending(&index_snapshot, cx);
            });
        }
    }

    fn clear_pending_index_edits_and_hunks(&mut self, cx: &mut Context<Self>) {
        self.clear_pending_index_edits();
        self.clear_pending_hunks(cx);
    }

    fn handle_base_texts_updated(
        &mut self,
        buffer: text::BufferSnapshot,
        message: proto::UpdateDiffBases,
        cx: &mut Context<Self>,
    ) {
        use proto::update_diff_bases::Mode;

        let Some(mode) = Mode::try_from(message.mode).ok() else {
            return;
        };

        let diff_bases_change = match mode {
            Mode::HeadOnly => Some(DiffBasesChange::SetHead(message.committed_text)),
            Mode::IndexOnly => Some(DiffBasesChange::SetIndex(message.staged_text)),
            Mode::IndexMatchesHead => Some(DiffBasesChange::SetBoth(message.committed_text)),
            Mode::IndexAndHead => Some(DiffBasesChange::SetEach {
                index: message.staged_text,
                head: message.committed_text,
            }),
            Mode::Unchanged => None,
        };

        self.diff_bases_changed(buffer, diff_bases_change, cx);
    }

    pub fn wait_for_recalculation(&mut self) -> Option<impl Future<Output = ()> + use<>> {
        if *self.recalculating_tx.borrow() {
            let mut rx = self.recalculating_tx.subscribe();
            Some(async move {
                loop {
                    let is_recalculating = rx.recv().await;
                    if is_recalculating != Some(true) {
                        break;
                    }
                }
            })
        } else {
            None
        }
    }

    fn diff_bases_changed(
        &mut self,
        buffer: text::BufferSnapshot,
        diff_bases_change: Option<DiffBasesChange>,
        cx: &mut Context<Self>,
    ) {
        match diff_bases_change {
            Some(DiffBasesChange::SetIndex(index)) => {
                self.index_text = index.map(|mut index| {
                    text::LineEnding::normalize(&mut index);
                    Arc::from(index.as_str())
                });
                self.index_changed = true;
            }
            Some(DiffBasesChange::SetHead(head)) => {
                self.head_text = head.map(|mut head| {
                    text::LineEnding::normalize(&mut head);
                    Arc::from(head.as_str())
                });
                self.head_changed = true;
            }
            Some(DiffBasesChange::SetBoth(text)) => {
                let text = text.map(|mut text| {
                    text::LineEnding::normalize(&mut text);
                    Arc::from(text.as_str())
                });
                self.head_text = text.clone();
                self.index_text = text;
                self.head_changed = true;
                self.index_changed = true;
            }
            Some(DiffBasesChange::SetEach { index, head }) => {
                self.index_text = index.map(|mut index| {
                    text::LineEnding::normalize(&mut index);
                    Arc::from(index.as_str())
                });
                self.index_changed = true;
                self.head_text = head.map(|mut head| {
                    text::LineEnding::normalize(&mut head);
                    Arc::from(head.as_str())
                });
                self.head_changed = true;
            }
            None => {}
        }

        self.recalculate_diffs(buffer, cx)
    }

    #[ztracing::instrument(skip_all)]
    fn recalculate_diffs(&mut self, buffer: text::BufferSnapshot, cx: &mut Context<Self>) {
        *self.recalculating_tx.borrow_mut() = true;

        let language = self.language.clone();
        let language_registry = self.language_registry.clone();
        let unstaged_diff = self.unstaged_diff();
        let staged_diff = self.staged_diff();
        let uncommitted_diff = self.uncommitted_diff();
        let head = self.head_text.clone();
        let index = self.index_text.clone();
        let head_text_buffer = self.head_text_buffer.upgrade();
        let index_text_buffer = self.index_text_buffer.upgrade();
        let index_text_buffer_language_enabled = self.index_text_buffer_language_enabled;
        let index_changed = self.index_changed;
        let head_changed = self.head_changed;
        let language_changed = self.language_changed;
        let prev_hunk_staging_operation_count = self.hunk_staging_operation_count_as_of_write;
        let index_matches_head = self.index_matches_head();

        let oid_diffs: Vec<(
            Option<git::Oid>,
            Entity<BufferDiff>,
            Entity<Buffer>,
            Option<Arc<str>>,
        )> = self
            .oid_diffs
            .iter()
            .filter_map(|(oid, weak)| {
                let diff = weak.upgrade()?;
                let base_text_buffer = diff.read(cx).base_text_buffer().clone();
                let base_text = match oid {
                    Some(oid) => Some(self.oid_texts.get(oid)?.clone()),
                    None => None,
                };
                Some((*oid, diff, base_text_buffer, base_text))
            })
            .collect();

        self.oid_diffs.retain(|oid, weak| {
            let alive = weak.upgrade().is_some();
            if !alive {
                if let Some(oid) = oid {
                    self.oid_texts.remove(oid);
                }
            }
            alive
        });
        if self
            .staged_diff
            .as_ref()
            .is_some_and(|(weak, _)| !weak.is_upgradable())
        {
            self.staged_diff = None;
        }
        self.recalculate_diff_task = Some(cx.spawn(async move |this, cx| {
            log::debug!(
                "start recalculating diffs for buffer {}",
                buffer.remote_id()
            );

            if index_text_buffer_language_enabled
                && let Some(index_text_buffer) = &index_text_buffer
            {
                index_text_buffer.update(cx, |index_text_buffer, cx| {
                    if let Some(language_registry) = language_registry.clone() {
                        index_text_buffer.set_language_registry(language_registry);
                    }
                    index_text_buffer.set_language_async(language.clone(), cx);
                });
            }
            if let Some(head_text_buffer) = &head_text_buffer {
                head_text_buffer.update(cx, |head_text_buffer, cx| {
                    if let Some(language_registry) = language_registry.clone() {
                        head_text_buffer.set_language_registry(language_registry);
                    }
                    head_text_buffer.set_language_async(language.clone(), cx);
                });
            }

            for (_, _, base_text_buffer, _) in &oid_diffs {
                base_text_buffer.update(cx, |base_text_buffer, cx| {
                    if let Some(language_registry) = language_registry.clone() {
                        base_text_buffer.set_language_registry(language_registry);
                    }
                    base_text_buffer.set_language_async(language.clone(), cx);
                });
            }

            let mut edited_index_text = None;

            let index_text_snapshot = if let Some(index_text_buffer) = &index_text_buffer
                && (unstaged_diff.is_some() || staged_diff.is_some())
            {
                let index_text_snapshot = if index_changed || language_changed {
                    let new_index_text = index.clone().unwrap_or_default();
                    let index_text_diff = index_text_buffer
                        .update(cx, |index_text_buffer, cx| {
                            index_text_buffer.diff(new_index_text.clone(), cx)
                        })
                        .await;
                    let edited = index_text_buffer
                        .update(cx, |index_text_buffer, cx| {
                            index_text_buffer.snapshot_with_edits(index_text_diff.edits, cx)
                        })
                        .await;
                    let snapshot = edited.snapshot().clone();
                    edited_index_text = Some(edited);
                    snapshot
                } else {
                    index_text_buffer.read_with(cx, |buffer, _| buffer.snapshot())
                };
                Some(index_text_snapshot)
            } else {
                None
            };

            let mut new_unstaged_diff = None;

            if let (Some(unstaged_diff), Some(index_text_snapshot)) =
                (unstaged_diff.as_ref(), index_text_snapshot.as_ref())
            {
                new_unstaged_diff = Some(
                    cx.update(|cx| {
                        unstaged_diff.read(cx).update_diff(
                            buffer.clone(),
                            index_text_snapshot,
                            index.clone(),
                            cx,
                        )
                    })
                    .await,
                );
            }

            // Dropping BufferDiff can be expensive, so yield back to the event loop
            // for a bit
            yield_now().await;

            let mut edited_head_text = None;
            let mut new_staged_diff = None;
            let mut new_uncommitted_diff = None;
            if let Some(head_text_buffer) = &head_text_buffer
                && (staged_diff.is_some() || uncommitted_diff.is_some())
            {
                let head_base_text_exists = head.is_some();
                let head_text_snapshot = if head_changed || language_changed {
                    let new_head_text = head.clone().unwrap_or_default();
                    let head_text_diff = head_text_buffer
                        .update(cx, |head_text_buffer, cx| {
                            head_text_buffer.diff(new_head_text.clone(), cx)
                        })
                        .await;
                    let edited = head_text_buffer
                        .update(cx, |base_text_buffer, cx| {
                            base_text_buffer.snapshot_with_edits(head_text_diff.edits, cx)
                        })
                        .await;
                    let snapshot = edited.snapshot().clone();
                    edited_head_text = Some(edited);
                    snapshot
                } else {
                    head_text_buffer.read_with(cx, |buffer, _| buffer.snapshot())
                };
                if let (Some(staged_diff), Some(index_base_text_snapshot)) =
                    (staged_diff.as_ref(), index_text_snapshot.as_ref())
                {
                    new_staged_diff = Some(
                        cx.update(|cx| {
                            staged_diff.read(cx).update_diff(
                                index_base_text_snapshot.text.clone(),
                                &head_text_snapshot,
                                head.clone(),
                                cx,
                            )
                        })
                        .await,
                    );
                }

                if let Some(uncommitted_diff) = &uncommitted_diff {
                    new_uncommitted_diff = if index_matches_head {
                        new_unstaged_diff.clone().map(|mut update| {
                            update.set_base_text_snapshot(
                                head_text_snapshot.clone(),
                                head_base_text_exists,
                            );
                            update
                        })
                    } else {
                        None
                    };
                    if new_uncommitted_diff.is_none() {
                        new_uncommitted_diff = Some(
                            cx.update(|cx| {
                                uncommitted_diff.read(cx).update_diff(
                                    buffer.clone(),
                                    &head_text_snapshot,
                                    head.clone(),
                                    cx,
                                )
                            })
                            .await,
                        );
                    }
                }
            }

            // Dropping BufferDiff can be expensive, so yield back to the event loop
            // for a bit
            yield_now().await;

            let cancel = this.update(cx, |this, _| {
                // This checks whether all pending stage/unstage operations
                // have quiesced (i.e. both the corresponding write and the
                // read of that write have completed). If not, then we cancel
                // this recalculation attempt to avoid invalidating pending
                // state too quickly; another recalculation will come along
                // later and clear the pending state once the state of the index has settled.
                if this.hunk_staging_operation_count > prev_hunk_staging_operation_count {
                    *this.recalculating_tx.borrow_mut() = false;
                    true
                } else {
                    false
                }
            })?;
            if cancel {
                log::debug!(
                    concat!(
                        "aborting recalculating diffs for buffer {}",
                        "due to subsequent hunk operations",
                    ),
                    buffer.remote_id()
                );
                return Ok(());
            }

            this.update(cx, |this, cx| {
                this.clear_pending_index_edits();

                if let (Some(staged_diff), Some(new_staged_diff)) =
                    (staged_diff.as_ref(), new_staged_diff.clone())
                {
                    staged_diff.update(cx, |diff, cx| {
                        if let Some(edited_base_text) = edited_index_text.take()
                            && let Some(index_text_buffer) = &index_text_buffer
                        {
                            index_text_buffer.update(cx, |index_text_buffer, cx| {
                                index_text_buffer.fast_forward(edited_base_text, cx)
                            });
                        }
                        if let Some(edited_head_text) = edited_head_text.take()
                            && let Some(head_text_buffer) = &head_text_buffer
                        {
                            head_text_buffer.update(cx, |head_text_buffer, cx| {
                                head_text_buffer.fast_forward(edited_head_text, cx)
                            });
                        }
                        diff.set_snapshot_with_secondary(new_staged_diff, None, true, cx)
                    });
                }

                let unstaged_changed_range = if let (Some(unstaged_diff), Some(new_unstaged_diff)) =
                    (unstaged_diff.as_ref(), new_unstaged_diff.clone())
                {
                    Some(unstaged_diff.update(cx, |diff, cx| {
                        if let Some(edited_index_text) = edited_index_text.take()
                            && let Some(index_text_buffer) = &index_text_buffer
                        {
                            index_text_buffer.update(cx, |index_text_buffer, cx| {
                                index_text_buffer.fast_forward(edited_index_text, cx)
                            });
                        }
                        diff.set_snapshot_with_secondary(new_unstaged_diff, None, true, cx)
                    }))
                } else {
                    None
                };

                if let (Some(uncommitted_diff), Some(new_uncommitted_diff)) =
                    (uncommitted_diff.as_ref(), new_uncommitted_diff.clone())
                {
                    uncommitted_diff.update(cx, |diff, cx| {
                        if let Some(edited_base_text) = edited_head_text.take()
                            && let Some(head_text_buffer) = &head_text_buffer
                        {
                            head_text_buffer.update(cx, |head_text_buffer, cx| {
                                head_text_buffer.fast_forward(edited_base_text, cx)
                            });
                        }
                        diff.set_snapshot_with_secondary(
                            new_uncommitted_diff,
                            unstaged_changed_range.flatten(),
                            true,
                            cx,
                        )
                    });
                }
            })?;

            yield_now().await;

            for (oid, oid_diff, base_text_buffer, base_text) in oid_diffs {
                let base_text_snapshot =
                    base_text_buffer.read_with(cx, |buffer, _| buffer.snapshot());
                let new_oid_diff = cx
                    .update(|cx| {
                        oid_diff.read(cx).update_diff(
                            buffer.clone(),
                            &base_text_snapshot,
                            base_text.clone(),
                            cx,
                        )
                    })
                    .await;

                oid_diff.update(cx, |diff, cx| diff.set_snapshot(new_oid_diff, cx));

                log::debug!(
                    "finished recalculating oid diff for buffer {} oid {:?}",
                    buffer.remote_id(),
                    oid
                );

                yield_now().await;
            }

            log::debug!(
                "finished recalculating diffs for buffer {}",
                buffer.remote_id()
            );

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, _| {
                    this.index_changed = false;
                    this.head_changed = false;
                    this.language_changed = false;
                    *this.recalculating_tx.borrow_mut() = false;
                });
            }

            Ok(())
        }));
    }
}

fn make_remote_delegate(
    this: Entity<GitStore>,
    project_id: u64,
    repository_id: RepositoryId,
    askpass_id: u64,
    cx: &mut AsyncApp,
) -> AskPassDelegate {
    AskPassDelegate::new(cx, move |prompt, tx, cx| {
        this.update(cx, |this, cx| {
            let Some((client, _)) = this.downstream_client() else {
                return;
            };
            let response = client.request(proto::AskPassRequest {
                project_id,
                repository_id: repository_id.to_proto(),
                askpass_id,
                prompt,
            });
            cx.spawn(async move |_, _| {
                let mut response = response.await?.response;
                tx.send(EncryptedPassword::try_from(response.as_ref())?)
                    .ok();
                response.zeroize();
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
    })
}

async fn request_remote_password(
    delegates: &RemoteAskPassDelegates,
    askpass_id: u64,
    prompt: String,
) -> Result<EncryptedPassword> {
    let (password_request, operation_cancellation) = {
        let mut delegates = delegates.lock();
        let delegate = delegates
            .get_mut(&askpass_id)
            .context("remote Git operation no longer exists")?;
        if delegate.active_request_cancellation.is_some() {
            bail!("another askpass request is already active");
        }

        let (cancellation_sender, cancellation_receiver) = oneshot::channel();
        let password_request = delegate.delegate.ask_password(prompt);
        delegate.active_request_cancellation = Some(cancellation_sender);
        (password_request.fuse(), cancellation_receiver.fuse())
    };

    futures::pin_mut!(password_request, operation_cancellation);
    let response = futures::select_biased! {
        response = password_request => response,
        _ = operation_cancellation => None,
    };

    let mut delegates = delegates.lock();
    let delegate = delegates
        .get_mut(&askpass_id)
        .context("remote Git operation ended while awaiting askpass")?;
    delegate.active_request_cancellation.take();

    response.context("askpass cancelled")
}

/// A history rewrite with `--autostash` that worked, except that git could not put the stashed
/// changes back afterwards and kept them as a stash entry instead ("Applying autostash resulted in
/// conflicts"), which git reports as success.
#[derive(Debug)]
pub struct ChangesLeftInStash;

impl std::fmt::Display for ChangesLeftInStash {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&i18n::t!(
            "Some changes could not be put back, so they were kept in the stash (stash@{{0}}). Restore them from the stash in the Git panel."
        ))
    }
}

impl std::error::Error for ChangesLeftInStash {}

/// A remote Zed from before `ls-files` was allowed refuses the check every history rewrite makes
/// for ignored files, which leaves the rewrite unusable against it; said in words the user can
/// act on, still refusing.
/// The `RpcError` tag that says a remote commit was made unsigned for
/// [`git::repository::CommittedWithoutSignature`], so the client need not recognize the sentence,
/// which the remote server writes in its own language.
const COMMITTED_WITHOUT_SIGNATURE_TAG: &str = "committed_without_signature";

fn tag_committed_without_signature(error: anyhow::Error) -> anyhow::Error {
    use rpc::ErrorCodeExt as _;
    if error
        .downcast_ref::<git::repository::CommittedWithoutSignature>()
        .is_some()
    {
        rpc::ErrorCode::Internal
            .message(format!("{error:#}"))
            .with_tag(COMMITTED_WITHOUT_SIGNATURE_TAG, "1")
            .anyhow()
    } else {
        error
    }
}

/// A remote server from before [`COMMITTED_WITHOUT_SIGNATURE_TAG`] sends only the sentence, which
/// [`git::repository::is_committed_without_signature`] still recognizes.
fn untag_committed_without_signature(error: anyhow::Error) -> anyhow::Error {
    if rpc::ErrorExt::error_tag(&error, COMMITTED_WITHOUT_SIGNATURE_TAG).is_some() {
        git::repository::CommittedWithoutSignature.into()
    } else {
        error
    }
}

fn explain_old_server_refusing_ls_files(error: anyhow::Error) -> anyhow::Error {
    if git::repository::is_unknown_git_subcommand_refusal(&format!("{error:#}"), "ls-files") {
        anyhow!(i18n::t!(
            "The Zed server on the remote host is too old for this rewrite. Update Zed on the remote host."
        ))
    } else {
        error
    }
}

/// The commits a fixup replays, as `Repository::validate_fixup_target_in_job` found them.
struct FixupRange {
    /// The target, as a full SHA.
    sha: String,
    /// `None` when the target is the repository's first commit.
    parent: Option<String>,
    head: String,
    /// `<parent>..HEAD`, or `HEAD` for a first commit: the target and everything after it.
    range: String,
}

impl RepositoryId {
    pub fn to_proto(self) -> u64 {
        self.0
    }

    pub fn from_proto(id: u64) -> Self {
        RepositoryId(id)
    }
}

impl RepositorySnapshot {
    fn empty(
        id: RepositoryId,
        work_directory_abs_path: Arc<Path>,
        repository_dir_abs_path: Option<Arc<Path>>,
        dot_git_abs_path: Option<Arc<Path>>,
        common_dir_abs_path: Option<Arc<Path>>,
        path_style: PathStyle,
    ) -> Self {
        let repository_dir_abs_path =
            repository_dir_abs_path.unwrap_or_else(|| work_directory_abs_path.join(".git").into());
        let dot_git_abs_path =
            dot_git_abs_path.unwrap_or_else(|| work_directory_abs_path.join(".git").into());
        let common_dir_abs_path =
            common_dir_abs_path.unwrap_or_else(|| repository_dir_abs_path.clone());

        Self {
            id,
            statuses_by_path: Default::default(),
            repository_dir_abs_path,
            dot_git_abs_path,
            common_dir_abs_path,
            work_directory_abs_path,
            branch: None,
            branch_list: Arc::from([]),
            branch_list_error: None,
            status_error: None,
            head_commit: None,
            scan_id: 0,
            merge: Default::default(),
            remote_origin_url: None,
            remote_upstream_url: None,
            stash_entries: Default::default(),
            linked_worktrees: Arc::from([]),
            path_style,
            loose_refs: None,
            tags_fingerprint: None,
            opened_by_user: false,
        }
    }

    fn initial_update(&self, project_id: u64) -> proto::UpdateRepository {
        proto::UpdateRepository {
            branch_summary: self.branch.as_ref().map(branch_to_proto),
            branch_list: self.branch_list.iter().map(branch_to_proto).collect(),
            branch_list_unchanged: false,
            branch_list_error: self
                .branch_list_error
                .as_ref()
                .map(|error| error.to_string()),
            head_commit_details: self.head_commit.as_ref().map(commit_details_to_proto),
            status_error: self.status_error.as_ref().map(|error| error.to_string()),
            updated_statuses: self
                .statuses_by_path
                .iter()
                .map(|entry| entry.to_proto())
                .collect(),
            removed_statuses: Default::default(),
            current_merge_conflicts: self
                .merge
                .merge_heads_by_conflicted_path
                .iter()
                .map(|(repo_path, _)| repo_path.as_unix_str().to_owned())
                .collect(),
            merge_message: self.merge.message.as_ref().map(|msg| msg.to_string()),
            project_id,
            id: self.id.to_proto(),
            abs_path: self.work_directory_abs_path.to_string_lossy().into_owned(),
            entry_ids: vec![self.id.to_proto()],
            scan_id: self.scan_id,
            is_last_update: true,
            stash_entries: self
                .stash_entries
                .entries
                .iter()
                .map(stash_to_proto)
                .collect(),
            remote_upstream_url: self.remote_upstream_url.clone(),
            remote_origin_url: self.remote_origin_url.clone(),
            repository_dir_abs_path: Some(
                self.repository_dir_abs_path.to_string_lossy().into_owned(),
            ),
            common_dir_abs_path: Some(self.common_dir_abs_path.to_string_lossy().into_owned()),
            linked_worktrees: self
                .linked_worktrees
                .iter()
                .map(worktree_to_proto)
                .collect(),
            tags_fingerprint: self.tags_fingerprint,
            loose_ref_count: self.loose_refs.map(|loose_refs| loose_refs.count as u64),
            loose_ref_count_is_lower_bound: self
                .loose_refs
                .is_some_and(|loose_refs| !loose_refs.complete),
            opened_by_user: self.opened_by_user,
        }
    }

    fn build_update(&self, old: &Self, project_id: u64) -> proto::UpdateRepository {
        let mut updated_statuses: Vec<proto::StatusEntry> = Vec::new();
        let mut removed_statuses: Vec<String> = Vec::new();

        let mut new_statuses = self.statuses_by_path.iter().peekable();
        let mut old_statuses = old.statuses_by_path.iter().peekable();

        let mut current_new_entry = new_statuses.next();
        let mut current_old_entry = old_statuses.next();
        loop {
            match (current_new_entry, current_old_entry) {
                (Some(new_entry), Some(old_entry)) => {
                    match new_entry.repo_path.cmp(&old_entry.repo_path) {
                        Ordering::Less => {
                            updated_statuses.push(new_entry.to_proto());
                            current_new_entry = new_statuses.next();
                        }
                        Ordering::Equal => {
                            if new_entry.status != old_entry.status
                                || new_entry.diff_stat != old_entry.diff_stat
                            {
                                updated_statuses.push(new_entry.to_proto());
                            }
                            current_old_entry = old_statuses.next();
                            current_new_entry = new_statuses.next();
                        }
                        Ordering::Greater => {
                            removed_statuses.push(old_entry.repo_path.as_unix_str().to_owned());
                            current_old_entry = old_statuses.next();
                        }
                    }
                }
                (None, Some(old_entry)) => {
                    removed_statuses.push(old_entry.repo_path.as_unix_str().to_owned());
                    current_old_entry = old_statuses.next();
                }
                (Some(new_entry), None) => {
                    updated_statuses.push(new_entry.to_proto());
                    current_new_entry = new_statuses.next();
                }
                (None, None) => break,
            }
        }

        // The branch list is the one part of a repository update that is always
        // sent in full. Repositories with many branches make that the bulk of
        // the message, and most updates (a file staged, a status refresh) leave
        // it untouched, so it is skipped when it did not change.
        let branch_list_unchanged = *self.branch_list == *old.branch_list
            && self.branch_list_error == old.branch_list_error;

        proto::UpdateRepository {
            branch_summary: self.branch.as_ref().map(branch_to_proto),
            branch_list: if branch_list_unchanged {
                Vec::new()
            } else {
                self.branch_list.iter().map(branch_to_proto).collect()
            },
            branch_list_unchanged,
            branch_list_error: if branch_list_unchanged {
                None
            } else {
                self.branch_list_error
                    .as_ref()
                    .map(|error| error.to_string())
            },
            head_commit_details: self.head_commit.as_ref().map(commit_details_to_proto),
            status_error: self.status_error.as_ref().map(|error| error.to_string()),
            updated_statuses,
            removed_statuses,
            current_merge_conflicts: self
                .merge
                .merge_heads_by_conflicted_path
                .iter()
                .map(|(path, _)| path.as_unix_str().to_owned())
                .collect(),
            merge_message: self.merge.message.as_ref().map(|msg| msg.to_string()),
            project_id,
            id: self.id.to_proto(),
            abs_path: self.work_directory_abs_path.to_string_lossy().into_owned(),
            entry_ids: vec![],
            scan_id: self.scan_id,
            is_last_update: true,
            stash_entries: self
                .stash_entries
                .entries
                .iter()
                .map(stash_to_proto)
                .collect(),
            remote_upstream_url: self.remote_upstream_url.clone(),
            remote_origin_url: self.remote_origin_url.clone(),
            repository_dir_abs_path: Some(
                self.repository_dir_abs_path.to_string_lossy().into_owned(),
            ),
            common_dir_abs_path: Some(self.common_dir_abs_path.to_string_lossy().into_owned()),
            linked_worktrees: self
                .linked_worktrees
                .iter()
                .map(worktree_to_proto)
                .collect(),
            tags_fingerprint: self.tags_fingerprint,
            loose_ref_count: self.loose_refs.map(|loose_refs| loose_refs.count as u64),
            loose_ref_count_is_lower_bound: self
                .loose_refs
                .is_some_and(|loose_refs| !loose_refs.complete),
            opened_by_user: self.opened_by_user,
        }
    }

    /// Returns the main worktree path for this repository, if one exists.
    ///
    /// Linked worktrees attached to bare repositories do not have a main
    /// worktree. For linked worktrees attached to a non-bare repository, the
    /// common Git directory is the main worktree's `.git` directory.
    pub fn main_worktree_abs_path(&self) -> Option<&Path> {
        if self.is_linked_worktree() {
            if self.common_dir_abs_path.file_name()? == std::ffi::OsStr::new(".git") {
                self.common_dir_abs_path.parent()
            } else {
                None
            }
        } else {
            Some(self.work_directory_abs_path.as_ref())
        }
    }

    /// The main worktree is the original checkout that other worktrees were
    /// created from.
    ///
    /// For example, if you had both `~/code/zed` and `~/code/worktrees/zed-2`,
    /// then `~/code/zed` is the main worktree and `~/code/worktrees/zed-2` is a linked worktree.
    ///
    /// Submodules also return `true` here, since they are not linked worktrees.
    pub fn is_main_worktree(&self) -> bool {
        !self.is_linked_worktree()
    }

    /// Returns true if this repository is a linked worktree, that is, one that
    /// was created from another worktree.
    ///
    /// Returns `false` for both the main worktree and submodules.
    pub fn is_linked_worktree(&self) -> bool {
        self.repository_dir_abs_path != self.common_dir_abs_path
    }

    pub fn linked_worktrees(&self) -> &[GitWorktree] {
        &self.linked_worktrees
    }

    pub fn status(&self) -> impl Iterator<Item = StatusEntry> + '_ {
        self.statuses_by_path.iter().cloned()
    }

    pub fn status_summary(&self) -> GitSummary {
        self.statuses_by_path.summary().item_summary
    }

    pub fn status_for_path(&self, path: &RepoPath) -> Option<StatusEntry> {
        self.statuses_by_path
            .get(&PathKey(path.as_ref().clone()), ())
            .cloned()
    }

    pub fn diff_stat_for_path(&self, path: &RepoPath) -> Option<DiffStat> {
        self.statuses_by_path
            .get(&PathKey(path.as_ref().clone()), ())
            .and_then(|entry| entry.diff_stat)
    }

    pub fn abs_path_to_repo_path(&self, abs_path: &Path) -> Option<RepoPath> {
        Self::abs_path_to_repo_path_inner(&self.work_directory_abs_path, abs_path, self.path_style)
    }

    pub fn repo_path_to_abs_path(&self, repo_path: &RepoPath) -> PathBuf {
        let repo_path = repo_path.display(self.path_style);
        PathBuf::from(
            self.path_style
                .join(&self.work_directory_abs_path, repo_path.as_ref())
                .unwrap(),
        )
    }

    #[inline]
    fn abs_path_to_repo_path_inner(
        work_directory_abs_path: &Path,
        abs_path: &Path,
        path_style: PathStyle,
    ) -> Option<RepoPath> {
        let rel_path = path_style.strip_prefix(abs_path, work_directory_abs_path)?;
        Some(RepoPath::from_rel_path(&rel_path))
    }

    pub fn had_conflict_on_last_merge_head_change(&self, repo_path: &RepoPath) -> bool {
        self.merge
            .merge_heads_by_conflicted_path
            .contains_key(repo_path)
    }

    pub fn has_conflict(&self, repo_path: &RepoPath) -> bool {
        let had_conflict_on_last_merge_head_change = self
            .merge
            .merge_heads_by_conflicted_path
            .contains_key(repo_path);
        let has_conflict_currently = self
            .status_for_path(repo_path)
            .is_some_and(|entry| entry.status.is_conflicted());
        had_conflict_on_last_merge_head_change || has_conflict_currently
    }

    /// This is the name that will be displayed in the repository selector for this repository.
    pub fn display_name(&self) -> SharedString {
        self.work_directory_abs_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
            .into()
    }
}

pub fn stash_to_proto(entry: &StashEntry) -> proto::StashEntry {
    proto::StashEntry {
        oid: entry.oid.as_bytes().to_vec(),
        message: entry.message.clone(),
        branch: entry.branch.clone(),
        index: entry.index as u64,
        timestamp: entry.timestamp,
    }
}

pub fn proto_to_stash(entry: &proto::StashEntry) -> Result<StashEntry> {
    Ok(StashEntry {
        oid: Oid::from_bytes(&entry.oid)?,
        message: entry.message.clone(),
        index: entry.index as usize,
        branch: entry.branch.clone(),
        timestamp: entry.timestamp,
    })
}

impl MergeDetails {
    async fn update(
        &mut self,
        backend: &Arc<dyn GitRepository>,
        current_conflicted_paths: Vec<RepoPath>,
    ) -> bool {
        log::debug!("load merge details");
        self.message = backend.merge_message().await.map(SharedString::from);
        let heads = backend
            .revparse_batch(vec![
                "MERGE_HEAD".into(),
                "CHERRY_PICK_HEAD".into(),
                "REBASE_HEAD".into(),
                "REVERT_HEAD".into(),
                "APPLY_HEAD".into(),
            ])
            .await
            .log_err()
            .unwrap_or_default()
            .into_iter()
            .map(|opt| opt.map(SharedString::from))
            .collect::<Vec<_>>();

        let mut conflicts_changed = false;

        // Record the merge state for newly conflicted paths
        for path in &current_conflicted_paths {
            if self.merge_heads_by_conflicted_path.get(&path).is_none() {
                conflicts_changed = true;
                self.merge_heads_by_conflicted_path
                    .insert(path.clone(), heads.clone());
            }
        }

        // Clear state for paths that are no longer conflicted and for which the merge heads have changed
        self.merge_heads_by_conflicted_path
            .retain(|path, old_merge_heads| {
                let keep = current_conflicted_paths.contains(path)
                    || (old_merge_heads == &heads
                        && old_merge_heads.iter().any(|head| head.is_some()));
                if !keep {
                    conflicts_changed = true;
                }
                keep
            });

        conflicts_changed
    }
}

impl Repository {
    pub fn is_trusted(&self) -> bool {
        match self.repository_state.peek() {
            Some(Ok(RepositoryState::Local(state))) => state.backend.is_trusted(),
            _ => false,
        }
    }

    pub fn snapshot(&self) -> RepositorySnapshot {
        self.snapshot.clone()
    }

    pub fn pending_ops(&self) -> impl Iterator<Item = PendingOps> + '_ {
        self.pending_ops.iter().cloned()
    }

    pub fn pending_ops_summary(&self) -> PathSummary<PendingOpsSummary> {
        self.pending_ops.summary().clone()
    }

    pub fn pending_ops_for_path(&self, path: &RepoPath) -> Option<PendingOps> {
        self.pending_ops
            .get(&PathKey(path.as_ref().clone()), ())
            .cloned()
    }

    fn respawn_local_worker(
        &mut self,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        is_trusted: bool,
        cx: &mut Context<Self>,
    ) {
        let work_directory_abs_path = self.snapshot.work_directory_abs_path.clone();
        let dot_git_abs_path = self.snapshot.dot_git_abs_path.clone();

        let state = cx
            .spawn(async move |this, cx| {
                let state = LocalRepositoryState::new(
                    work_directory_abs_path,
                    dot_git_abs_path,
                    project_environment,
                    fs,
                    is_trusted,
                    cx,
                )
                .await
                // `{:#}` keeps the cause (e.g. "no git binary available"), which
                // the git panel shows when the repository cannot be opened.
                .map_err(|err| format!("{err:#}"))?;
                // Before the state is handed to the worker, so that no command runs as
                // anyone else (ADR-F93).
                let identity = this
                    .read_with(cx, |repository, cx| {
                        let applies = repository
                            .git_store
                            .upgrade()
                            .is_some_and(|git_store| git_store.read(cx).applies_git_accounts());
                        repository.git_account_identity(applies, cx)
                    })
                    .ok()
                    .flatten();
                state.backend.set_identity(identity);
                Ok(state)
            })
            .shared();
        self.job_sender.close_channel();
        self._worker_task = Task::ready(());
        self.active_jobs.clear();
        self.job_debug_queue
            .mark_unfinished_complete(job_debug_queue::CompletedJobStatus::Skipped);
        cx.notify();

        let (job_sender, worker_task) = Repository::spawn_local_git_worker(state.clone(), cx);
        self.job_sender = job_sender;
        self._worker_task = worker_task;
        self.repository_state = cx
            .spawn(async move |_, _| {
                let state = state.await?;
                Ok(RepositoryState::Local(state))
            })
            .shared();
    }

    /// ForgeKairos (ADR-F93): the identity this repository's git commands run as.
    fn git_account_identity(
        &self,
        applies: bool,
        cx: &App,
    ) -> Option<Arc<git::account::GitIdentity>> {
        if !applies {
            // A remote server's user settings are the client's, keyed by its paths; the client
            // tells it the account instead.
            return self.identity_from_client.clone();
        }
        crate::git_accounts::resolve_identity(
            &ProjectSettings::get_global(cx).git,
            &self.snapshot.work_directory_abs_path,
        )
    }

    fn apply_git_account(&mut self, applies: bool, cx: &mut Context<Self>) -> Task<()> {
        let identity = self.git_account_identity(applies, cx);
        let state = self.repository_state.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(RepositoryState::Local(state)) = state.await
                && state.backend.identity() != identity
            {
                state.backend.set_identity(identity);
                this.update(cx, |_, cx| cx.notify()).ok();
            }
        })
    }

    /// ADR-F93: tells the remote server the account picked for this repository under
    /// `connection`, unless it was told that already.
    fn send_remote_git_account(&mut self, connection: &str, cx: &mut Context<Self>) {
        let key = crate::git_accounts::remote_project_account_key(
            connection,
            &self.snapshot.work_directory_abs_path,
        );
        let identity = crate::git_accounts::resolve_identity_for_key(
            &ProjectSettings::get_global(cx).git,
            &key,
        );
        if self.remote_git_account.sent() == Some(&identity) {
            return;
        }
        let id = self.id;
        let state = self.repository_state.clone();
        self.remote_git_account =
            crate::git_accounts::RemoteGitAccount::send(id, identity, state, cx);
        cx.notify();
    }

    /// ADR-F93: what the remote server was told of this repository's git account, and whether
    /// it took it. `None` until the host has been told (and always for a local repository).
    pub fn remote_git_account_status(
        &self,
    ) -> Option<&crate::git_accounts::RemoteGitAccountStatus> {
        self.remote_git_account.status()
    }

    /// ADR-F93: for a remote repository, the account whose HTTPS login the client answers the
    /// host's git prompts for (the one the host took), and whether the user trusts the
    /// repository; the token stays on the client and only answers the prompt.
    fn remote_account_for_askpass(
        &self,
        cx: &mut App,
    ) -> (Option<Arc<git::account::GitIdentity>>, bool) {
        let Some(Some(identity)) = self.remote_git_account.sent().cloned() else {
            return (None, false);
        };
        if self.remote_git_account.status()
            != Some(&crate::git_accounts::RemoteGitAccountStatus::Applied)
        {
            return (None, false);
        }
        let Some(worktree_store) = self
            .git_store
            .upgrade()
            .map(|git_store| git_store.read(cx).worktree_store.clone())
        else {
            return (None, false);
        };
        let is_trusted = GitStore::remote_repository_is_trusted(
            &worktree_store,
            &self.work_directory_abs_path,
            cx,
        );
        (Some(identity), is_trusted)
    }

    fn reinitialize_local_backend(
        &mut self,
        work_directory_abs_path: Arc<Path>,
        dot_git_abs_path: Arc<Path>,
        repository_dir_abs_path: Arc<Path>,
        common_dir_abs_path: Arc<Path>,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        is_trusted: bool,
        cx: &mut Context<Self>,
    ) {
        self.snapshot.work_directory_abs_path = work_directory_abs_path;
        self.snapshot.dot_git_abs_path = dot_git_abs_path;
        self.snapshot.repository_dir_abs_path = repository_dir_abs_path;
        self.snapshot.common_dir_abs_path = common_dir_abs_path;
        self.respawn_local_worker(project_environment, fs, is_trusted, cx);
    }

    fn local(
        id: RepositoryId,
        work_directory_abs_path: Arc<Path>,
        repository_dir_abs_path: Arc<Path>,
        common_dir_abs_path: Arc<Path>,
        dot_git_abs_path: Arc<Path>,
        project_environment: WeakEntity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        is_trusted: bool,
        git_store: WeakEntity<GitStore>,
        blob_read_limiter: Arc<Semaphore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = RepositorySnapshot::empty(
            id,
            work_directory_abs_path,
            Some(repository_dir_abs_path),
            Some(dot_git_abs_path),
            Some(common_dir_abs_path),
            PathStyle::local(),
        );

        let mut repo = Repository {
            this: cx.weak_entity(),
            git_store,
            blob_read_limiter,
            snapshot,
            unshallow_state: UnshallowState::default(),
            pending_ops: Default::default(),
            repository_state: Task::ready(Err("not yet initialized".into())).shared(),
            _worker_task: Task::ready(()),
            commit_message_buffer: None,
            askpass_delegates: Default::default(),
            paths_needing_status_update: Default::default(),
            pending_snapshot_scan: SnapshotScan::NONE,
            latest_askpass_id: 0,
            job_sender: mpsc::unbounded().0,
            job_id: 0,
            active_jobs: Default::default(),
            job_debug_queue: job_debug_queue::GitJobDebugQueue::new(),
            initial_graph_data: Default::default(),
            commit_data: Default::default(),
            commit_data_handler: CommitDataHandlerState::Closed,
            identity_from_client: None,
            remote_git_account: Default::default(),
        };
        repo.respawn_local_worker(project_environment, fs, is_trusted, cx);
        cx.subscribe_self(Self::handle_subscribe_self).detach();
        repo
    }

    fn remote(
        id: RepositoryId,
        work_directory_abs_path: Arc<Path>,
        repository_dir_abs_path: Option<Arc<Path>>,
        common_dir_abs_path: Option<Arc<Path>>,
        path_style: PathStyle,
        project_id: ProjectId,
        client: AnyProtoClient,
        git_store: WeakEntity<GitStore>,
        blob_read_limiter: Arc<Semaphore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let snapshot = RepositorySnapshot::empty(
            id,
            work_directory_abs_path,
            repository_dir_abs_path,
            None,
            common_dir_abs_path,
            path_style,
        );

        let repository_state = RemoteRepositoryState { project_id, client };
        let (job_sender, worker_task) = Self::spawn_remote_git_worker(repository_state.clone(), cx);
        let repository_state = Task::ready(Ok(RepositoryState::Remote(repository_state))).shared();
        cx.subscribe_self(Self::handle_subscribe_self).detach();

        Self {
            this: cx.weak_entity(),
            snapshot,
            unshallow_state: UnshallowState::default(),
            commit_message_buffer: None,
            git_store,
            blob_read_limiter,
            pending_ops: Default::default(),
            paths_needing_status_update: Default::default(),
            pending_snapshot_scan: SnapshotScan::NONE,
            job_sender,
            _worker_task: worker_task,
            repository_state,
            askpass_delegates: Default::default(),
            latest_askpass_id: 0,
            active_jobs: Default::default(),
            job_debug_queue: job_debug_queue::GitJobDebugQueue::new(),
            job_id: 0,
            initial_graph_data: Default::default(),
            commit_data: Default::default(),
            commit_data_handler: CommitDataHandlerState::Closed,
            identity_from_client: None,
            remote_git_account: Default::default(),
        }
    }

    fn handle_subscribe_self(&mut self, event: &RepositoryEvent, _: &mut Context<Self>) {
        // scan id greater than 2 means the initial snapshot was calculated,
        // otherwise we don't need to refresh the graph state
        match event {
            // The graph's cached log carries the tags that decorate its commits, so it goes
            // stale when a tag is added or deleted too.
            RepositoryEvent::HeadChanged
            | RepositoryEvent::BranchListChanged
            | RepositoryEvent::TagsChanged => {
                if self.scan_id > 2 {
                    self.initial_graph_data.clear();
                }
            }
            RepositoryEvent::StashEntriesChanged => {
                if self.scan_id > 2 {
                    self.initial_graph_data
                        .retain(|(log_source, _), _| *log_source != LogSource::All);
                }
            }
            _ => {}
        }
    }

    pub fn git_store(&self) -> Option<Entity<GitStore>> {
        self.git_store.upgrade()
    }

    fn reload_buffer_diff_bases(&mut self, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let git_store = self.git_store.clone();
        let _ = self.send_keyed_job(
            "reload_buffer_diff_bases",
            Some(GitJobKey::ReloadBufferDiffBases),
            None,
            |state, mut cx| async move {
                let RepositoryState::Local(LocalRepositoryState { backend, .. }) = state else {
                    log::error!("tried to recompute diffs for a non-local repository");
                    return Ok(());
                };

                let Some(this) = this.upgrade() else {
                    return Ok(());
                };

                let repo_diff_state_updates = this.update(&mut cx, |this, cx| {
                    git_store.update(cx, |git_store, cx| {
                        git_store
                            .diffs
                            .iter()
                            .filter_map(|(buffer_id, diff_state)| {
                                let buffer_store = git_store.buffer_store.read(cx);
                                let buffer = buffer_store.get(*buffer_id)?;
                                let file = File::from_dyn(buffer.read(cx).file())?;
                                let abs_path = file.worktree.read(cx).absolutize(&file.path);
                                let repo_path = this.abs_path_to_repo_path(&abs_path)?;
                                let is_symlink = GitStore::file_is_symlink(file, cx);
                                log::debug!(
                                    "start reload diff bases for repo path {}",
                                    repo_path.as_unix_str()
                                );
                                diff_state.update(cx, |diff_state, _| {
                                    let has_unstaged_diff = diff_state
                                        .unstaged_diff
                                        .as_ref()
                                        .is_some_and(|diff| diff.is_upgradable());
                                    let has_staged_diff = diff_state
                                        .staged_diff
                                        .as_ref()
                                        .is_some_and(|(diff, _)| diff.is_upgradable());
                                    let has_uncommitted_diff = diff_state
                                        .uncommitted_diff
                                        .as_ref()
                                        .is_some_and(|set| set.is_upgradable());

                                    Some((
                                        buffer,
                                        repo_path,
                                        is_symlink,
                                        (has_unstaged_diff || has_staged_diff)
                                            .then(|| diff_state.index_text.clone()),
                                        (has_staged_diff || has_uncommitted_diff)
                                            .then(|| diff_state.head_text.clone()),
                                    ))
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                })?;

                let buffer_diff_base_changes = cx
                    .background_spawn(async move {
                        let mut revisions = Vec::new();
                        for (_, repo_path, is_symlink, current_index_text, current_head_text) in
                            &repo_diff_state_updates
                        {
                            if current_index_text.is_some() && !*is_symlink {
                                revisions.push(format!(":{}", repo_path.as_unix_str()));
                            }
                            if current_head_text.is_some() && !*is_symlink {
                                revisions.push(format!("HEAD:{}", repo_path.as_unix_str()));
                            }
                        }

                        let mut loaded_revisions = backend
                            .load_revisions(revisions)
                            .await
                            .log_err()
                            .into_iter()
                            .flatten();

                        let mut changes = Vec::new();
                        for (buffer, _, is_symlink, current_index_text, current_head_text) in
                            &repo_diff_state_updates
                        {
                            let index_text = (current_index_text.is_some() && !*is_symlink)
                                .then(|| loaded_revisions.next().flatten())
                                .flatten()
                                .and_then(|bytes| decode_git_text(bytes).log_err());

                            let head_text = (current_head_text.is_some() && !*is_symlink)
                                .then(|| loaded_revisions.next().flatten())
                                .flatten()
                                .and_then(|bytes| decode_git_text(bytes).log_err());

                            let change =
                                match (current_index_text.as_ref(), current_head_text.as_ref()) {
                                    (Some(current_index), Some(current_head)) => {
                                        let index_changed =
                                            index_text.as_deref() != current_index.as_deref();
                                        let head_changed =
                                            head_text.as_deref() != current_head.as_deref();
                                        if index_changed && head_changed {
                                            if index_text == head_text {
                                                Some(DiffBasesChange::SetBoth(head_text))
                                            } else {
                                                Some(DiffBasesChange::SetEach {
                                                    index: index_text,
                                                    head: head_text,
                                                })
                                            }
                                        } else if index_changed {
                                            Some(DiffBasesChange::SetIndex(index_text))
                                        } else if head_changed {
                                            Some(DiffBasesChange::SetHead(head_text))
                                        } else {
                                            None
                                        }
                                    }
                                    (Some(current_index), None) => {
                                        let index_changed =
                                            index_text.as_deref() != current_index.as_deref();
                                        index_changed
                                            .then_some(DiffBasesChange::SetIndex(index_text))
                                    }
                                    (None, Some(current_head)) => {
                                        let head_changed =
                                            head_text.as_deref() != current_head.as_deref();
                                        head_changed.then_some(DiffBasesChange::SetHead(head_text))
                                    }
                                    (None, None) => None,
                                };

                            changes.push((buffer.clone(), change))
                        }
                        changes
                    })
                    .await;

                git_store.update(&mut cx, |git_store, cx| {
                    for (buffer, diff_bases_change) in buffer_diff_base_changes {
                        let buffer_snapshot = buffer.read(cx).text_snapshot();
                        let buffer_id = buffer_snapshot.remote_id();
                        let Some(diff_state) = git_store.diffs.get(&buffer_id) else {
                            continue;
                        };

                        let downstream_client = git_store.downstream_client();
                        diff_state.update(cx, |diff_state, cx| {
                            use proto::update_diff_bases::Mode;

                            if let Some((client, project_id)) = downstream_client {
                                let (staged_text, committed_text, mode) =
                                    match diff_bases_change.clone() {
                                        Some(DiffBasesChange::SetIndex(index)) => {
                                            (index, None, Mode::IndexOnly)
                                        }
                                        Some(DiffBasesChange::SetHead(head)) => {
                                            (None, head, Mode::HeadOnly)
                                        }
                                        Some(DiffBasesChange::SetEach { index, head }) => {
                                            (index, head, Mode::IndexAndHead)
                                        }
                                        Some(DiffBasesChange::SetBoth(text)) => {
                                            (None, text, Mode::IndexMatchesHead)
                                        }
                                        None => (None, None, Mode::Unchanged),
                                    };
                                client
                                    .send(proto::UpdateDiffBases {
                                        project_id: project_id.to_proto(),
                                        buffer_id: buffer_id.to_proto(),
                                        staged_text,
                                        committed_text,
                                        mode: mode as i32,
                                    })
                                    .log_err();
                            }

                            diff_state.diff_bases_changed(buffer_snapshot, diff_bases_change, cx);
                        });
                    }
                })
            },
        );
    }

    pub fn send_job<F, Fut, R>(
        &mut self,
        description: &'static str,
        status: Option<SharedString>,
        job: F,
    ) -> oneshot::Receiver<R>
    where
        F: FnOnce(RepositoryState, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        self.send_keyed_job(description, None, status, job)
    }

    fn send_keyed_job<F, Fut, R>(
        &mut self,
        description: &'static str,
        key: Option<GitJobKey>,
        status: Option<SharedString>,
        job: F,
    ) -> oneshot::Receiver<R>
    where
        F: FnOnce(RepositoryState, AsyncApp) -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = futures::channel::oneshot::channel();
        let job_id = post_inc(&mut self.job_id);
        let this = self.this.clone();

        let key_label = key.as_ref().map(format_job_key);
        self.job_debug_queue.add(job_id, description, key_label);

        self.job_sender
            .unbounded_send(GitJob {
                id: job_id,
                key,
                job: Box::new(move |state, cx: &mut AsyncApp| {
                    let job = job(state, cx.clone());
                    cx.spawn(async move |cx| {
                        this.update(cx, |this, cx| {
                            this.job_debug_queue.mark_running(job_id);
                            if let Some(s) = status {
                                this.active_jobs.insert(
                                    job_id,
                                    JobInfo {
                                        start: Instant::now(),
                                        message: s,
                                    },
                                );
                            }
                            cx.notify();
                        })
                        .ok();

                        let result = job.await;

                        this.update(cx, |this, cx| {
                            this.job_debug_queue.mark_complete(
                                job_id,
                                job_debug_queue::CompletedJobStatus::Finished,
                            );
                            this.active_jobs.remove(&job_id);
                            cx.notify();
                        })
                        .ok();

                        result_tx.send(result).ok();
                    })
                }),
            })
            .ok();
        result_rx
    }

    pub fn set_as_active_repository(&self, cx: &mut Context<Self>) {
        let Some(git_store) = self.git_store.upgrade() else {
            return;
        };
        let entity = cx.entity();
        git_store.update(cx, |git_store, cx| {
            let Some((&id, _)) = git_store
                .repositories
                .iter()
                .find(|(_, handle)| *handle == &entity)
            else {
                return;
            };
            git_store.active_repo_id = Some(id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged(Some(id)));
        });
    }

    pub fn cached_status(&self) -> impl '_ + Iterator<Item = StatusEntry> {
        self.snapshot.status()
    }

    pub fn diff_stat_for_path(&self, path: &RepoPath) -> Option<DiffStat> {
        self.snapshot.diff_stat_for_path(path)
    }

    pub fn cached_stash(&self) -> GitStash {
        self.snapshot.stash_entries.clone()
    }

    pub fn repo_path_to_project_path(&self, path: &RepoPath, cx: &App) -> Option<ProjectPath> {
        let git_store = self.git_store.upgrade()?;
        let worktree_store = git_store.read(cx).worktree_store.read(cx);
        let abs_path = self.snapshot.repo_path_to_abs_path(path);
        let abs_path = SanitizedPath::new(&abs_path);
        let (worktree, relative_path) = worktree_store.find_worktree(abs_path, cx)?;
        Some(ProjectPath {
            worktree_id: worktree.read(cx).id(),
            path: relative_path,
        })
    }

    pub fn project_path_to_repo_path(&self, path: &ProjectPath, cx: &App) -> Option<RepoPath> {
        let git_store = self.git_store.upgrade()?;
        let worktree_store = git_store.read(cx).worktree_store.read(cx);
        let abs_path = worktree_store.absolutize(path, cx)?;
        self.snapshot.abs_path_to_repo_path(&abs_path)
    }

    pub fn contains_sub_repo(&self, other: &Entity<Self>, cx: &App) -> bool {
        other
            .read(cx)
            .snapshot
            .work_directory_abs_path
            .starts_with(&self.snapshot.work_directory_abs_path)
    }

    pub fn commit_message_buffer(&self) -> Option<&Entity<Buffer>> {
        self.commit_message_buffer.as_ref()
    }

    pub fn open_commit_buffer(
        &mut self,
        languages: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        let id = self.id;
        if let Some(buffer) = self.commit_message_buffer.clone() {
            return Task::ready(Ok(buffer));
        }
        let this = cx.weak_entity();

        let rx = self.send_job(
            "open_commit_buffer",
            None,
            move |state, mut cx| async move {
                let Some(this) = this.upgrade() else {
                    bail!("git store was dropped");
                };
                match state {
                    RepositoryState::Local(..) => {
                        this.update(&mut cx, |_, cx| {
                            Self::open_local_commit_buffer(languages, buffer_store, cx)
                        })
                        .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let request = client.request(proto::OpenCommitMessageBuffer {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        });
                        let response = request.await.context("requesting to open commit buffer")?;
                        let buffer_id = BufferId::new(response.buffer_id)?;
                        let buffer = buffer_store
                            .update(&mut cx, |buffer_store, cx| {
                                buffer_store.wait_for_remote_buffer(buffer_id, cx)
                            })
                            .await?;
                        if let Some(language_registry) = languages {
                            let git_commit_language =
                                language_registry.language_for_name("Git Commit").await?;
                            buffer.update(&mut cx, |buffer, cx| {
                                buffer.set_language(Some(git_commit_language), cx);
                            });
                        }
                        this.update(&mut cx, |this, _| {
                            this.commit_message_buffer = Some(buffer.clone());
                        });
                        Ok(buffer)
                    }
                }
            },
        );

        cx.spawn(|_, _: &mut AsyncApp| async move { rx.await? })
    }

    fn open_local_commit_buffer(
        language_registry: Option<Arc<LanguageRegistry>>,
        buffer_store: Entity<BufferStore>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<Buffer>>> {
        cx.spawn(async move |repository, cx| {
            let git_commit_language = match language_registry {
                Some(language_registry) => {
                    Some(language_registry.language_for_name("Git Commit").await?)
                }
                None => None,
            };
            let buffer = buffer_store
                .update(cx, |buffer_store, cx| {
                    buffer_store.create_buffer(git_commit_language, false, cx)
                })
                .await?;

            repository.update(cx, |repository, _| {
                repository.commit_message_buffer = Some(buffer.clone());
            })?;
            Ok(buffer)
        })
    }

    pub fn checkout_files(
        &mut self,
        commit: &str,
        paths: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let commit = commit.to_string();
        let id = self.id;

        self.spawn_job_with_tracking(
            paths.clone(),
            pending_op::GitStatus::Reverted,
            cx,
            async move |this, cx| {
                this.update(cx, |this, _cx| {
                    this.send_job(
                        "checkout_files",
                        Some(format!("git checkout {}", commit).into()),
                        move |git_repo, _| async move {
                            match git_repo {
                                RepositoryState::Local(LocalRepositoryState {
                                    backend,
                                    environment,
                                    ..
                                }) => {
                                    backend
                                        .checkout_files(commit, paths, environment.clone())
                                        .await
                                }
                                RepositoryState::Remote(RemoteRepositoryState {
                                    project_id,
                                    client,
                                }) => {
                                    client
                                        .request(proto::GitCheckoutFiles {
                                            project_id: project_id.0,
                                            repository_id: id.to_proto(),
                                            commit,
                                            paths: paths
                                                .into_iter()
                                                .map(|p| p.as_unix_str().to_owned())
                                                .collect(),
                                        })
                                        .await?;

                                    Ok(())
                                }
                            }
                        },
                    )
                })?
                .await?
            },
        )
    }

    pub fn reset(
        &mut self,
        commit: String,
        reset_mode: ResetMode,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;

        let receiver = self.send_job("reset", None, move |git_repo, _| async move {
            match git_repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => backend.reset(commit, reset_mode, environment).await,
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    client
                        .request(proto::GitReset {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            commit,
                            mode: match reset_mode {
                                ResetMode::Soft => git_reset::ResetMode::Soft.into(),
                                ResetMode::Mixed => git_reset::ResetMode::Mixed.into(),
                                ResetMode::Hard => git_reset::ResetMode::Hard.into(),
                            },
                        })
                        .await?;

                    Ok(())
                }
            }
        });

        let scan_updates_tx =
            self.git_store()
                .and_then(|git_store| match &git_store.read(cx).state {
                    GitStoreState::Local { downstream, .. } => Some(
                        downstream
                            .as_ref()
                            .map(|downstream| downstream.updates_tx.clone()),
                    ),
                    _ => None,
                });
        if let Some(updates_tx) = scan_updates_tx {
            self.schedule_scan(updates_tx, cx);
        }

        receiver
    }

    pub fn show(&mut self, commit: String) -> oneshot::Receiver<Result<CommitDetails>> {
        let id = self.id;
        self.send_job("show", None, move |git_repo, _cx| async move {
            match git_repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.show(commit).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let resp = client
                        .request(proto::GitShow {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            commit,
                        })
                        .await?;

                    Ok(CommitDetails {
                        sha: resp.sha.into(),
                        message: resp.message.into(),
                        commit_timestamp: resp.commit_timestamp,
                        author_email: resp.author_email.into(),
                        author_name: resp.author_name.into(),
                    })
                }
            }
        })
    }

    pub fn load_commit_diff(
        &mut self,
        commit: String,
        ignore_shallow_boundary: bool,
    ) -> oneshot::Receiver<Result<CommitDiff>> {
        let id = self.id;
        self.send_job("load_commit_diff", None, move |git_repo, cx| async move {
            match git_repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => backend
                    .load_commit(commit, ignore_shallow_boundary, cx)
                    .await
                    .map(decode_commit_diff),
                RepositoryState::Remote(RemoteRepositoryState {
                    client, project_id, ..
                }) => {
                    let response = client
                        .request(proto::LoadCommitDiff {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            commit,
                            ignore_shallow_boundary,
                        })
                        .await?;
                    Ok(CommitDiff {
                        files: response
                            .files
                            .into_iter()
                            .map(|file| {
                                Ok(CommitFile {
                                    path: RepoPath::from_proto(&file.path)?,
                                    old_text: file.old_text,
                                    new_text: file.new_text,
                                    is_binary: file.is_binary,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?,
                        is_shallow_boundary: response.is_shallow_boundary,
                    })
                }
            }
        })
    }

    pub fn file_history_changed_files(
        &mut self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> oneshot::Receiver<Result<Vec<FileHistoryChangedFileSets>>> {
        self.send_job(
            "file_history_changed_files",
            None,
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend
                            .file_history_changed_files(paths, commit_limit)
                            .await
                    }
                    RepositoryState::Remote(_) => {
                        anyhow::bail!("file history changed files is only supported locally")
                    }
                }
            },
        )
    }

    pub fn get_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
    ) -> Option<&InitialGitGraphData> {
        self.initial_graph_data.get(&(log_source, log_order))
    }

    /// Streams the commits matching `search_args` into `request_tx`. Dropping the task stops
    /// the search; its error says why a search could not run.
    pub fn search_commits(
        &mut self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: async_channel::Sender<Oid>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let repository_state = self.repository_state.clone();
        let repository_id = self.id;

        cx.background_spawn(async move {
            match repository_state.await {
                Ok(RepositoryState::Local(LocalRepositoryState { backend, .. })) => {
                    backend
                        .search_commits(log_source, search_args, request_tx)
                        .await
                }
                Ok(RepositoryState::Remote(RemoteRepositoryState { client, project_id })) => {
                    let requires_filters = search_args.uses_filters_beyond_author();
                    let search_id = NEXT_COMMIT_SEARCH_ID.fetch_add(1, atomic::Ordering::Relaxed);
                    let mut request = search_commit_args_to_proto(
                        project_id.to_proto(),
                        repository_id.to_proto(),
                        &log_source,
                        &search_args,
                    );
                    request.search_id = Some(search_id);
                    let server_cancels = Arc::new(AtomicBool::new(false));
                    // Runs when the search is dropped before the server finished it (stopped,
                    // replaced, or past the limit), so that the server's git stops as well.
                    // An older server would not know the message, so it only goes to one that
                    // said it takes it.
                    let cancel_on_drop = util::defer({
                        let client = client.clone();
                        let server_cancels = server_cancels.clone();
                        move || {
                            if server_cancels.load(atomic::Ordering::Relaxed) {
                                client
                                    .send(proto::CancelSearchCommits {
                                        project_id: project_id.to_proto(),
                                        repository_id: repository_id.to_proto(),
                                        search_id,
                                    })
                                    .log_err();
                            }
                        }
                    });
                    let stream = client.request_stream(request).await?;
                    let end = forward_remote_commit_search(
                        stream,
                        requires_filters,
                        &server_cancels,
                        request_tx,
                    )
                    .await;
                    if matches!(end, Ok(RemoteCommitSearchEnd::ServerFinished)) {
                        cancel_on_drop.abort();
                    }
                    end.map(|_| ())
                }
                Err(error) => Err(anyhow!(error)),
            }
        })
    }

    pub fn graph_data(
        &mut self,
        log_source: LogSource,
        log_order: LogOrder,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> GraphDataResponse<'_> {
        let initial_commit_data = self
            .initial_graph_data
            .entry((log_source.clone(), log_order))
            .or_insert_with(|| {
                let state = self.repository_state.clone();
                let log_source = log_source.clone();

                let fetch_task = cx.spawn(async move |repository, cx| {
                    let state = state.await;
                    let result = match state {
                        Ok(RepositoryState::Local(LocalRepositoryState { backend, .. })) => {
                            Self::local_git_graph_data(
                                repository.clone(),
                                backend,
                                log_source.clone(),
                                log_order,
                                cx,
                            )
                            .await
                        }
                        Ok(RepositoryState::Remote(remote)) => {
                            Self::remote_git_graph_data(
                                repository.clone(),
                                remote,
                                log_source.clone(),
                                log_order,
                                cx,
                            )
                            .await
                        }
                        Err(e) => Err(SharedString::from(e)),
                    };

                    repository
                        .update(cx, |repository, cx| {
                            if let Some(data) = repository
                                .initial_graph_data
                                .get_mut(&(log_source.clone(), log_order))
                            {
                                match &result {
                                    Ok(()) => {
                                        cx.emit(RepositoryEvent::GraphEvent(
                                            (log_source.clone(), log_order),
                                            GitGraphEvent::FullyLoaded,
                                        ));
                                    }
                                    Err(fetch_task_error) => {
                                        data.subscribers.retain(|sender| {
                                            sender.try_send(Err(fetch_task_error.clone())).is_ok()
                                        });
                                        data.error = Some(fetch_task_error.clone());
                                        cx.emit(RepositoryEvent::GraphEvent(
                                            (log_source.clone(), log_order),
                                            GitGraphEvent::LoadingError,
                                        ));
                                    }
                                }
                                data.subscribers.clear();
                            } else {
                                debug_panic!(
                                    "This task would be dropped if this entry doesn't exist"
                                );
                            }
                        })
                        .log_err();
                });

                InitialGitGraphData {
                    fetch_task,
                    error: None,
                    commit_data: Vec::new(),
                    commit_oid_to_index: HashMap::default(),
                    subscribers: Vec::new(),
                }
            });

        let max_start = initial_commit_data.commit_data.len().saturating_sub(1);
        let max_end = initial_commit_data.commit_data.len();

        GraphDataResponse {
            commits: &initial_commit_data.commit_data
                [range.start.min(max_start)..range.end.min(max_end)],
            is_loading: !initial_commit_data.fetch_task.is_ready(),
            error: initial_commit_data.error.clone(),
        }
    }

    async fn append_initial_graph_commits(
        this: &WeakEntity<Self>,
        graph_data_key: &(LogSource, LogOrder),
        initial_graph_commit_data: Vec<Arc<InitialGraphCommitData>>,
        cx: &mut AsyncApp,
    ) {
        this.update(cx, |repository, cx| {
            let graph_data = repository
                .initial_graph_data
                .entry(graph_data_key.clone())
                .and_modify(|graph_data| {
                    if !graph_data.subscribers.is_empty() {
                        graph_data.subscribers.retain(|sender| {
                            sender
                                .try_send(Ok(initial_graph_commit_data.clone()))
                                .is_ok()
                        });
                    }

                    for commit_data in initial_graph_commit_data {
                        graph_data
                            .commit_oid_to_index
                            .insert(commit_data.sha, graph_data.commit_data.len());
                        graph_data.commit_data.push(commit_data);
                    }
                    cx.emit(RepositoryEvent::GraphEvent(
                        graph_data_key.clone(),
                        GitGraphEvent::CountUpdated(graph_data.commit_data.len()),
                    ));
                });

            match &graph_data {
                Entry::Occupied(_) => {}
                Entry::Vacant(_) => {
                    debug_panic!("This task should be dropped if data doesn't exist");
                }
            }
        })
        .log_err();
    }

    async fn local_git_graph_data(
        this: WeakEntity<Self>,
        backend: Arc<dyn GitRepository>,
        log_source: LogSource,
        log_order: LogOrder,
        cx: &mut AsyncApp,
    ) -> Result<(), SharedString> {
        let (request_tx, request_rx) =
            async_channel::unbounded::<Vec<Arc<InitialGraphCommitData>>>();

        let task = cx.background_executor().spawn({
            let log_source = log_source.clone();
            async move {
                backend
                    .initial_graph_data(log_source, log_order, request_tx)
                    .await
                    .map_err(|err| SharedString::from(err.to_string()))
            }
        });

        let graph_data_key = (log_source, log_order);

        while let Ok(initial_graph_commit_data) = request_rx.recv().await {
            Self::append_initial_graph_commits(
                &this,
                &graph_data_key,
                initial_graph_commit_data,
                cx,
            )
            .await;
        }

        task.await?;
        Ok(())
    }

    async fn remote_git_graph_data(
        this: WeakEntity<Self>,
        remote: RemoteRepositoryState,
        log_source: LogSource,
        log_order: LogOrder,
        cx: &mut AsyncApp,
    ) -> Result<(), SharedString> {
        let repository_id = this
            .update(cx, |repository, _| repository.id)
            .map_err(|err| SharedString::from(err.to_string()))?;
        let graph_data_key = (log_source.clone(), log_order);
        let mut response = remote
            .client
            .request_stream(proto::GetInitialGraphData {
                project_id: remote.project_id.to_proto(),
                repository_id: repository_id.to_proto(),
                log_source: Some(log_source_to_proto(&log_source)),
                log_order: log_order_to_proto(log_order),
            })
            .await
            .map_err(|err| SharedString::from(err.to_string()))?;

        while let Some(response) = response.next().await {
            let response = response.map_err(|err| SharedString::from(err.to_string()))?;
            let commits = response
                .commits
                .into_iter()
                .map(initial_graph_commit_from_proto)
                .collect::<Result<Vec<_>>>()
                .map_err(|err| SharedString::from(err.to_string()))?;
            Self::append_initial_graph_commits(&this, &graph_data_key, commits, cx).await;
        }

        Ok(())
    }

    pub fn fetch_commit_data(
        &mut self,
        sha: Oid,
        await_result: bool,
        cx: &mut Context<Self>,
    ) -> &CommitDataState {
        if self.commit_data.contains_key(&sha) {
            let data = &self.commit_data[&sha];

            if let CommitDataState::Loading(None) = data
                && await_result
            {
                let (tx, rx) = oneshot::channel();
                self.commit_data
                    .insert(sha, CommitDataState::Loading(Some(rx.shared())));

                let handler = self.get_handler(cx);
                handler.completion_senders.insert(sha, tx);
            }

            return &self.commit_data[&sha];
        }

        let (state, completer) = if await_result {
            let (tx, rx) = oneshot::channel();
            (CommitDataState::Loading(Some(rx.shared())), Some(tx))
        } else {
            (CommitDataState::Loading(None), None)
        };

        self.commit_data.insert(sha, state);

        let handler = self.get_handler(cx);
        if let Some(tx) = completer {
            handler.completion_senders.insert(sha, tx);
        }
        let mut has_failed = false;
        if handler.commit_data_request.try_send(sha).is_ok() {
            handler.pending_requests.insert(sha);
        } else {
            has_failed = true;
            handler.completion_senders.remove(&sha);
            debug_assert!(
                matches!(
                    self.commit_data.remove(&sha),
                    Some(CommitDataState::Loading(_))
                ),
                "Commit data should still be loading when enqueueing the request fails"
            );
        }

        &self.commit_data.get(&sha).unwrap_or_else(|| {
            debug_assert!(!has_failed, "This should always be inserted");
            &CommitDataState::Loading(None)
        })
    }

    fn get_handler(&mut self, cx: &mut Context<Self>) -> &mut CommitDataHandler {
        if matches!(self.commit_data_handler, CommitDataHandlerState::Closed) {
            self.commit_data_handler =
                CommitDataHandlerState::Open(self.open_commit_data_handler(cx));
        }

        match &mut self.commit_data_handler {
            CommitDataHandlerState::Open(handler) => handler,
            CommitDataHandlerState::Closed => unreachable!(),
        }
    }

    fn open_commit_data_handler(&self, cx: &Context<Self>) -> CommitDataHandler {
        let state = self.repository_state.clone();
        let (result_tx, result_rx) = async_channel::bounded::<(Oid, CommitData)>(64);
        let (request_tx, request_rx) = async_channel::unbounded::<Oid>();

        let foreground_task = cx.spawn(async move |this, cx| {
            while let Ok((sha, commit_data)) = result_rx.recv().await {
                let result = this.update(cx, |this, cx| {
                    let data = Arc::new(commit_data);

                    if let CommitDataHandlerState::Open(handler) = &mut this.commit_data_handler {
                        handler.pending_requests.remove(&sha);
                        if let Some(completion_sender) = handler.completion_senders.remove(&sha) {
                            completion_sender.send(data.clone()).ok();
                        }
                    } else {
                        debug_panic!("The handler state has to be open for this task to exist");
                    }

                    let old_value = this.commit_data.insert(sha, CommitDataState::Loaded(data));
                    debug_assert!(
                        !matches!(old_value, Some(CommitDataState::Loaded(_))),
                        "We should never overwrite commit data"
                    );

                    cx.notify();
                });
                if result.is_err() {
                    break;
                }
            }

            this.update(cx, |this, _cx| {
                let CommitDataHandlerState::Open(handler) = std::mem::replace(
                    &mut this.commit_data_handler,
                    CommitDataHandlerState::Closed,
                ) else {
                    debug_panic!("The handler state has to be open for this task to exist");
                    return;
                };

                for sha in handler.pending_requests {
                    this.commit_data.remove(&sha);
                }
            })
            .ok();
        });

        let request_tx_for_handler = request_tx;
        let repository_id = self.id;
        let background_executor = cx.background_executor().clone();

        cx.background_spawn(async move {
            match state.await {
                Ok(RepositoryState::Local(LocalRepositoryState { backend, .. })) => {
                    Self::local_commit_data_reader(
                        backend,
                        request_rx,
                        result_tx,
                        background_executor,
                    )
                    .await;
                }
                Ok(RepositoryState::Remote(RemoteRepositoryState { project_id, client })) => {
                    Self::remote_commit_data_reader(
                        project_id,
                        client,
                        repository_id,
                        request_rx,
                        result_tx,
                        background_executor,
                    )
                    .await;
                }
                Err(error) => {
                    log::error!("failed to get repository state: {error}");
                    return;
                }
            };
        })
        .detach();

        CommitDataHandler {
            _task: foreground_task,
            commit_data_request: request_tx_for_handler,
            completion_senders: HashMap::default(),
            pending_requests: HashSet::default(),
        }
    }

    async fn local_commit_data_reader(
        backend: Arc<dyn GitRepository>,
        request_rx: smol::channel::Receiver<Oid>,
        result_tx: smol::channel::Sender<(Oid, CommitData)>,
        background_executor: BackgroundExecutor,
    ) {
        async fn receive_commit_data_request(
            request_rx: &smol::channel::Receiver<Oid>,
        ) -> Option<Oid> {
            if request_rx.is_closed() && request_rx.is_empty() {
                future::pending().await
            } else {
                request_rx.recv().await.ok()
            }
        }

        let reader = match backend.commit_data_reader() {
            Ok(reader) => reader,
            Err(error) => {
                log::error!("failed to create commit data reader: {error:?}");
                return;
            }
        };

        let read_commit_data = |sha| reader.read(sha).map(move |result| (sha, result));
        let mut read_futures = FuturesUnordered::new();

        loop {
            if read_futures.is_empty() {
                let timeout = background_executor.timer(Duration::from_secs(10));

                futures::select_biased! {
                    sha = futures::FutureExt::fuse(receive_commit_data_request(&request_rx)) => {
                        if let Some(sha) = sha {
                            read_futures.push(read_commit_data(sha));
                        }
                    }
                    _ = futures::FutureExt::fuse(timeout) => {
                        break;
                    }
                }
            }

            let next_read = read_futures.next().fuse();
            futures::pin_mut!(next_read);

            futures::select_biased! {
                result = next_read => {
                    let Some((sha, result)) = result else {
                        continue;
                    };

                    match result {
                        Ok(commit_data) => {
                            if result_tx.send((sha, commit_data)).await.is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            log::error!("failed to read commit data for {sha}: {error:?}");
                        }
                    }
                }
                sha = futures::FutureExt::fuse(receive_commit_data_request(&request_rx)) => {
                    if let Some(sha) = sha {
                        read_futures.push(read_commit_data(sha));
                    }
                }
            }
        }

        drop(result_tx);
    }

    async fn remote_commit_data_reader(
        project_id: ProjectId,
        client: AnyProtoClient,
        repository_id: RepositoryId,
        request_rx: smol::channel::Receiver<Oid>,
        result_tx: smol::channel::Sender<(Oid, CommitData)>,
        background_executor: BackgroundExecutor,
    ) {
        let mut response_futures =
            FuturesUnordered::<BoxFuture<'static, Result<proto::GetCommitDataResponse>>>::new();
        let mut accept_requests = true;
        let mut next_request = Self::get_next_request(
            project_id,
            client.clone(),
            repository_id,
            &request_rx,
            &background_executor,
        )
        .boxed()
        .fuse();

        loop {
            if !accept_requests && response_futures.is_empty() {
                break;
            }

            if response_futures.is_empty() {
                match (&mut next_request).await {
                    NextCommitDataRequest::Request(request) => {
                        response_futures.push(request);
                        next_request = Self::get_next_request(
                            project_id,
                            client.clone(),
                            repository_id,
                            &request_rx,
                            &background_executor,
                        )
                        .boxed()
                        .fuse();
                    }
                    NextCommitDataRequest::Closed | NextCommitDataRequest::Idle => break,
                }
            }

            let next_response = response_futures.next().fuse();
            futures::pin_mut!(next_response);

            futures::select_biased! {
                request = next_request => {
                    match request {
                        NextCommitDataRequest::Request(request) => {
                            response_futures.push(request);
                        }
                        NextCommitDataRequest::Idle => {}
                        NextCommitDataRequest::Closed => {
                            accept_requests = false;
                        }
                    }

                    if accept_requests {
                        next_request = Self::get_next_request(
                            project_id,
                            client.clone(),
                            repository_id,
                            &request_rx,
                            &background_executor,
                        )
                        .boxed()
                        .fuse();
                    }
                }
                result = next_response => {
                    let Some(result) = result else {
                        continue;
                    };

                    if let Ok(commit_data) = result {
                        for commit in commit_data.commits {
                            let Ok(commit_data) = commit_data_from_proto(commit) else {
                                continue;
                            };

                            if result_tx
                                .send((commit_data.sha, commit_data))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        }

        drop(result_tx);
    }

    async fn get_next_request(
        project_id: ProjectId,
        client: AnyProtoClient,
        repository_id: RepositoryId,
        request_rx: &smol::channel::Receiver<Oid>,
        background_executor: &BackgroundExecutor,
    ) -> NextCommitDataRequest {
        let mut queued_shas = Vec::with_capacity(64);

        loop {
            if queued_shas.len() >= 64 {
                break;
            }

            let timeout = background_executor.timer(Duration::from_millis(5));

            futures::select_biased! {
                sha = futures::FutureExt::fuse(request_rx.recv()) => {
                    let Ok(sha) = sha else {
                        break;
                    };

                    queued_shas.push(sha);

                }
                _ = futures::FutureExt::fuse(timeout) => {
                    break;
                }
            }
        }

        if queued_shas.is_empty() && request_rx.is_closed() {
            NextCommitDataRequest::Closed
        } else if queued_shas.is_empty() {
            NextCommitDataRequest::Idle
        } else {
            NextCommitDataRequest::Request(
                client
                    .request(proto::GetCommitData {
                        project_id: project_id.to_proto(),
                        repository_id: repository_id.to_proto(),
                        shas: queued_shas.into_iter().map(|oid| oid.to_string()).collect(),
                    })
                    .boxed(),
            )
        }
    }

    fn buffer_store(&self, cx: &App) -> Option<Entity<BufferStore>> {
        Some(self.git_store.upgrade()?.read(cx).buffer_store.clone())
    }

    fn save_buffers<'a>(
        &self,
        entries: impl IntoIterator<Item = &'a RepoPath>,
        cx: &mut Context<Self>,
    ) -> Vec<Task<anyhow::Result<()>>> {
        let mut save_futures = Vec::new();
        if let Some(buffer_store) = self.buffer_store(cx) {
            buffer_store.update(cx, |buffer_store, cx| {
                for path in entries {
                    let Some(project_path) = self.repo_path_to_project_path(path, cx) else {
                        continue;
                    };
                    if let Some(buffer) = buffer_store.get_by_path(&project_path)
                        && buffer
                            .read(cx)
                            .file()
                            .is_some_and(|file| file.disk_state().exists())
                        && buffer.read(cx).has_unsaved_edits()
                    {
                        save_futures.push(buffer_store.save_buffer(buffer, cx));
                    }
                }
            })
        }
        save_futures
    }

    pub fn stage_entries(
        &mut self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.stage_or_unstage_entries(true, entries, cx)
    }

    pub fn unstage_entries(
        &mut self,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.stage_or_unstage_entries(false, entries, cx)
    }

    fn stage_or_unstage_entries(
        &mut self,
        stage: bool,
        entries: Vec<RepoPath>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if entries.is_empty() {
            return Task::ready(Ok(()));
        }
        let Some(git_store) = self.git_store.upgrade() else {
            return Task::ready(Ok(()));
        };
        let id = self.id;
        let save_tasks = self.save_buffers(&entries, cx);
        let paths = entries
            .iter()
            .map(|p| p.as_unix_str())
            .collect::<Vec<_>>()
            .join(" ");
        let status = if stage {
            format!("git add {paths}")
        } else {
            format!("git reset {paths}")
        };
        let job_key = GitJobKey::WriteIndex(entries.clone());

        self.spawn_job_with_tracking(
            entries.clone(),
            if stage {
                pending_op::GitStatus::Staged
            } else {
                pending_op::GitStatus::Unstaged
            },
            cx,
            async move |this, cx| {
                for save_task in save_tasks {
                    save_task.await?;
                }

                this.update(cx, |this, cx| {
                    let weak_this = cx.weak_entity();
                    this.send_keyed_job(
                        "stage_or_unstage_entries",
                        Some(job_key),
                        Some(status.into()),
                        move |git_repo, mut cx| async move {
                            let hunk_staging_operation_counts = weak_this
                                .update(&mut cx, |this, cx| {
                                    let mut hunk_staging_operation_counts = HashMap::default();
                                    for path in &entries {
                                        let Some(project_path) =
                                            this.repo_path_to_project_path(path, cx)
                                        else {
                                            continue;
                                        };
                                        let Some(buffer) = git_store
                                            .read(cx)
                                            .buffer_store
                                            .read(cx)
                                            .get_by_path(&project_path)
                                        else {
                                            continue;
                                        };
                                        let Some(diff_state) = git_store
                                            .read(cx)
                                            .diffs
                                            .get(&buffer.read(cx).remote_id())
                                            .cloned()
                                        else {
                                            continue;
                                        };
                                        let buffer_snapshot = buffer.read(cx).text_snapshot();
                                        let hunk_staging_operation_count =
                                            diff_state.update(cx, |diff_state, cx| {
                                                diff_state
                                                    .mark_whole_file_stage_or_unstage_pending(
                                                        stage,
                                                        &buffer_snapshot,
                                                        cx,
                                                    );
                                                diff_state.hunk_staging_operation_count += 1;
                                                diff_state.hunk_staging_operation_count
                                            });
                                        hunk_staging_operation_counts.insert(
                                            diff_state.downgrade(),
                                            hunk_staging_operation_count,
                                        );
                                    }
                                    hunk_staging_operation_counts
                                })
                                .unwrap_or_default();

                            let result = match git_repo {
                                RepositoryState::Local(LocalRepositoryState {
                                    backend,
                                    environment,
                                    ..
                                }) => {
                                    if stage {
                                        backend.stage_paths(entries, environment.clone()).await
                                    } else {
                                        backend.unstage_paths(entries, environment.clone()).await
                                    }
                                }
                                RepositoryState::Remote(RemoteRepositoryState {
                                    project_id,
                                    client,
                                }) => {
                                    if stage {
                                        client
                                            .request(proto::Stage {
                                                project_id: project_id.0,
                                                repository_id: id.to_proto(),
                                                paths: entries
                                                    .into_iter()
                                                    .map(|repo_path| {
                                                        repo_path.as_unix_str().to_owned()
                                                    })
                                                    .collect(),
                                            })
                                            .await
                                            .context("sending stage request")
                                            .map(|_| ())
                                    } else {
                                        client
                                            .request(proto::Unstage {
                                                project_id: project_id.0,
                                                repository_id: id.to_proto(),
                                                paths: entries
                                                    .into_iter()
                                                    .map(|repo_path| {
                                                        repo_path.as_unix_str().to_owned()
                                                    })
                                                    .collect(),
                                            })
                                            .await
                                            .context("sending unstage request")
                                            .map(|_| ())
                                    }
                                }
                            };

                            for (diff_state, hunk_staging_operation_count) in
                                hunk_staging_operation_counts
                            {
                                diff_state
                                    .update(&mut cx, |diff_state, cx| {
                                        if result.is_ok() {
                                            diff_state.hunk_staging_operation_count_as_of_write =
                                                hunk_staging_operation_count;
                                        } else {
                                            diff_state.clear_pending_hunks(cx);
                                        }
                                    })
                                    .ok();
                            }

                            result
                        },
                    )
                })?
                .await?
            },
        )
    }

    pub fn stage_all(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let snapshot = self.snapshot.clone();
        let pending_ops = self.pending_ops.clone();
        let to_stage = cx.background_spawn(async move {
            snapshot
                .status()
                .filter_map(|entry| {
                    if let Some(ops) = pending_ops
                        .get(&PathKey(entry.repo_path.as_ref().clone()), ())
                        .filter(|ops| !ops.last_op_errored())
                    {
                        if ops.staging() || ops.staged() {
                            None
                        } else {
                            Some(entry.repo_path)
                        }
                    } else if entry.status.staging().is_fully_staged() {
                        None
                    } else {
                        Some(entry.repo_path)
                    }
                })
                .collect()
        });

        cx.spawn(async move |this, cx| {
            let to_stage = to_stage.await;
            this.update(cx, |this, cx| {
                this.stage_or_unstage_entries(true, to_stage, cx)
            })?
            .await
        })
    }

    pub fn unstage_all(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        let snapshot = self.snapshot.clone();
        let pending_ops = self.pending_ops.clone();
        let to_unstage = cx.background_spawn(async move {
            snapshot
                .status()
                .filter_map(|entry| {
                    if let Some(ops) = pending_ops
                        .get(&PathKey(entry.repo_path.as_ref().clone()), ())
                        .filter(|ops| !ops.last_op_errored())
                    {
                        if !ops.staging() && !ops.staged() {
                            None
                        } else {
                            Some(entry.repo_path)
                        }
                    } else if entry.status.staging().is_fully_unstaged() {
                        None
                    } else {
                        Some(entry.repo_path)
                    }
                })
                .collect()
        });

        cx.spawn(async move |this, cx| {
            let to_unstage = to_unstage.await;
            this.update(cx, |this, cx| {
                this.stage_or_unstage_entries(false, to_unstage, cx)
            })?
            .await
        })
    }

    pub fn stash_all(
        &mut self,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let to_stash = self.cached_status().map(|entry| entry.repo_path).collect();

        self.stash_entries(to_stash, message, cx)
    }

    pub fn stash_tracked(
        &mut self,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        // `is_created` rather than `is_untracked`, so that staging a new file does not
        // move it out of the untracked set: `git add` makes it `Tracked { Added }`, but
        // the panel still lists it under "Untracked" and the user expects it left alone.
        let to_stash: Vec<_> = self
            .cached_status()
            .filter(|entry| !entry.status.is_created())
            .map(|entry| entry.repo_path)
            .collect();

        // An empty pathspec would make `git stash push` stash everything.
        if to_stash.is_empty() {
            return Task::ready(Ok(()));
        }
        self.stash_entries(to_stash, message, cx)
    }

    pub fn stash_entries(
        &mut self,
        entries: Vec<RepoPath>,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let id = self.id;

        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job("stash_entries", None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local(LocalRepositoryState {
                            backend,
                            environment,
                            ..
                        }) => backend.stash_paths(entries, message, environment).await,
                        RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                            client
                                .request(proto::Stash {
                                    project_id: project_id.0,
                                    repository_id: id.to_proto(),
                                    paths: entries
                                        .into_iter()
                                        .map(|repo_path| repo_path.as_unix_str().to_owned())
                                        .collect(),
                                    message,
                                    staged: None,
                                })
                                .await?;
                            Ok(())
                        }
                    }
                })
            })?
            .await??;
            Ok(())
        })
    }

    pub fn stash_staged(
        &mut self,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let id = self.id;

        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job("stash_staged", None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local(LocalRepositoryState {
                            backend,
                            environment,
                            ..
                        }) => backend.stash_staged(message, environment).await,
                        RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                            client
                                .request(proto::Stash {
                                    project_id: project_id.0,
                                    repository_id: id.to_proto(),
                                    paths: Vec::new(),
                                    message,
                                    staged: Some(true),
                                })
                                .await?;
                            Ok(())
                        }
                    }
                })
            })?
            .await??;
            Ok(())
        })
    }

    pub fn stash_pop(
        &mut self,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let id = self.id;
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job("stash_pop", None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local(LocalRepositoryState {
                            backend,
                            environment,
                            ..
                        }) => backend.stash_pop(index, environment).await,
                        RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                            client
                                .request(proto::StashPop {
                                    project_id: project_id.0,
                                    repository_id: id.to_proto(),
                                    stash_index: index.map(|i| i as u64),
                                })
                                .await
                                .context("sending stash pop request")?;
                            Ok(())
                        }
                    }
                })
            })?
            .await??;
            Ok(())
        })
    }

    pub fn stash_apply(
        &mut self,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let id = self.id;
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, _| {
                this.send_job("stash_apply", None, move |git_repo, _cx| async move {
                    match git_repo {
                        RepositoryState::Local(LocalRepositoryState {
                            backend,
                            environment,
                            ..
                        }) => backend.stash_apply(index, environment).await,
                        RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                            client
                                .request(proto::StashApply {
                                    project_id: project_id.0,
                                    repository_id: id.to_proto(),
                                    stash_index: index.map(|i| i as u64),
                                })
                                .await
                                .context("sending stash apply request")?;
                            Ok(())
                        }
                    }
                })
            })?
            .await??;
            Ok(())
        })
    }

    pub fn add_path_to_gitignore(
        &mut self,
        repo_path: &RepoPath,
        is_dir: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let work_dir = self.snapshot.work_directory_abs_path.clone();
        let path_display = repo_path.as_ref().display(PathStyle::Unix);
        let path = repo_path.as_unix_str().to_owned();
        let file_path_str = if is_dir {
            format!("{}/", path_display)
        } else {
            path_display.to_string()
        };

        self.send_job(
            "add_path_to_gitignore",
            None,
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState { fs, .. }) => {
                        append_pattern_to_ignore_file(
                            fs,
                            work_dir.join(".gitignore"),
                            file_path_str,
                        )
                        .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitAddPathToGitignore {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                path,
                                is_dir,
                            })
                            .await
                            .context("sending add path to .gitignore request")?;
                        Ok(())
                    }
                }
            },
        )
    }

    pub fn add_path_to_git_info_exclude(
        &mut self,
        repo_path: &RepoPath,
        is_dir: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let repository_dir = self.snapshot.common_dir_abs_path.clone();
        let path_display = repo_path.as_ref().display(PathStyle::Unix);
        let path = repo_path.as_unix_str().to_owned();
        let file_path_str = if is_dir {
            format!("{}/", path_display)
        } else {
            path_display.to_string()
        };

        self.send_job(
            "add_path_to_git_info_exclude",
            None,
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState { fs, .. }) => {
                        append_pattern_to_ignore_file(
                            fs,
                            repository_dir.join(git::REPO_EXCLUDE),
                            file_path_str,
                        )
                        .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitAddPathToGitInfoExclude {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                path,
                                is_dir,
                            })
                            .await
                            .context("sending add path to .git/info/exclude request")?;
                        Ok(())
                    }
                }
            },
        )
    }

    pub fn stash_drop(
        &mut self,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let id = self.id;
        let updates_tx = self
            .git_store()
            .and_then(|git_store| match &git_store.read(cx).state {
                GitStoreState::Local { downstream, .. } => downstream
                    .as_ref()
                    .map(|downstream| downstream.updates_tx.clone()),
                _ => None,
            });
        let this = cx.weak_entity();
        self.send_job("stash_drop", None, move |git_repo, mut cx| async move {
            match git_repo {
                RepositoryState::Local(LocalRepositoryState {
                    backend,
                    environment,
                    ..
                }) => {
                    // TODO would be nice to not have to do this manually
                    let result = backend.stash_drop(index, environment).await;
                    if result.is_ok()
                        && let Ok(stash_entries) = backend.stash_entries().await
                    {
                        let snapshot = this.update(&mut cx, |this, cx| {
                            this.snapshot.stash_entries = stash_entries;
                            cx.emit(RepositoryEvent::StashEntriesChanged);
                            this.snapshot.clone()
                        })?;
                        if let Some(updates_tx) = updates_tx {
                            updates_tx
                                .unbounded_send(DownstreamUpdate::UpdateRepository(snapshot))
                                .ok();
                        }
                    }

                    result
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    client
                        .request(proto::StashDrop {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            stash_index: index.map(|i| i as u64),
                        })
                        .await
                        .context("sending stash pop request")?;
                    Ok(())
                }
            }
        })
    }

    // Kept for wire compatibility: older remote clients run the pre-commit hook explicitly
    // via `proto::RunGitHook` before committing. New code lets `git commit` run hooks itself.
    //
    // TODO: remove together with `proto::RunGitHook` once all supported peers commit without
    // sending it (see the deprecation note on the message in git.proto).
    pub fn run_hook(&mut self, hook: RunHook, _cx: &mut App) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "run_hook",
            Some(format!("git hook {}", hook.as_str()).into()),
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => backend.run_hook(hook, environment.clone()).await,
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::RunGitHook {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                hook: hook.to_proto(),
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn commit(
        &mut self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        askpass: AskPassDelegate,
        _cx: &mut App,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let account_applied = self.remote_git_account.applied.clone();

        self.send_job(
            "commit",
            Some("git commit".into()),
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        backend
                            .commit(message, name_and_email, options, askpass, environment)
                            .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        crate::git_accounts::ensure_remote_git_account_applied(account_applied)
                            .await?;
                        let _askpass_operation =
                            RemoteAskPassOperation::new(askpass_id, askpass, askpass_delegates);
                        let (name, email) = name_and_email.unzip();
                        client
                            .request(proto::Commit {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                message: String::from(message),
                                name: name.map(String::from),
                                email: email.map(String::from),
                                options: Some(proto::commit::CommitOptions {
                                    amend: options.amend,
                                    signoff: options.signoff,
                                    allow_empty: options.allow_empty,
                                    no_verify: options.no_verify,
                                }),
                                askpass_id,
                            })
                            .await
                            .map_err(untag_committed_without_signature)?;

                        Ok(())
                    }
                }
            },
        )
    }

    async fn refresh_branch_list(
        this: &WeakEntity<Self>,
        backend: Arc<dyn GitRepository>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let branches_scan = backend.branches().await?;
        let branch_list_error = branches_scan.error;
        let ref_storage = branches_scan.ref_storage;
        let branch_list: Arc<[Branch]> = branches_scan.branches.into();
        let branch = branch_list.iter().find(|branch| branch.is_head).cloned();
        log::info!("head branch after scan is {branch:?}");
        let snapshot = this.update(cx, |this, cx| {
            let head_changed = branch != this.snapshot.branch;
            let branch_list_changed = *branch_list != *this.snapshot.branch_list;
            let branch_list_error_changed = this.snapshot.branch_list_error != branch_list_error;
            this.snapshot.branch = branch;
            this.snapshot.branch_list = branch_list;
            this.snapshot.branch_list_error = branch_list_error;
            if head_changed {
                cx.emit(RepositoryEvent::HeadChanged);
            }
            if branch_list_changed || branch_list_error_changed {
                cx.emit(RepositoryEvent::BranchListChanged);
            }
            this.apply_ref_storage_scan(ref_storage, cx);
            this.snapshot.clone()
        })?;
        if let Some(updates_tx) = updates_tx {
            updates_tx
                .unbounded_send(DownstreamUpdate::UpdateRepository(snapshot))
                .ok();
        }
        Ok(())
    }

    /// Takes in what the branch listing read from the ref files: how many refs are loose, and
    /// whether a tag changed since the previous listing.
    fn apply_ref_storage_scan(&mut self, ref_storage: RefStorageScan, cx: &mut Context<Self>) {
        self.snapshot.loose_refs = ref_storage.loose_refs;
        self.set_tags_fingerprint(ref_storage.tags_fingerprint, cx);
    }

    fn set_tags_fingerprint(&mut self, tags_fingerprint: Option<u64>, cx: &mut Context<Self>) {
        let tags_changed =
            tags_fingerprint.is_some() && tags_fingerprint != self.snapshot.tags_fingerprint;
        self.snapshot.tags_fingerprint = tags_fingerprint;
        if tags_changed {
            cx.emit(RepositoryEvent::TagsChanged);
        }
    }

    pub fn unshallow_state(&self) -> UnshallowState {
        self.unshallow_state
    }

    pub fn fetch_unshallow(
        &mut self,
        askpass: AskPassDelegate,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let receiver = self.fetch(FetchOptions::Unshallow, askpass, cx);
        self.unshallow_state = UnshallowState::InProgress;
        cx.notify();
        let (tx, rx) = oneshot::channel();
        cx.spawn(async move |this, cx| {
            let result = receiver.await;
            let succeeded = matches!(&result, Ok(Ok(_)));
            this.update(cx, |this, cx| {
                this.unshallow_state = if succeeded {
                    UnshallowState::Unshallowed
                } else {
                    UnshallowState::Idle
                };
                cx.notify();
            })
            .ok();
            if let Ok(result) = result {
                tx.send(result).ok();
            }
        })
        .detach();
        rx
    }

    pub fn fetch(
        &mut self,
        fetch_options: FetchOptions,
        askpass: AskPassDelegate,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let id = self.id;

        let updates_tx = self
            .git_store()
            .and_then(|git_store| match &git_store.read(cx).state {
                GitStoreState::Local { downstream, .. } => downstream
                    .as_ref()
                    .map(|downstream| downstream.updates_tx.clone()),
                _ => None,
            });

        let this = cx.weak_entity();
        let (remote_identity, remote_is_trusted) = self.remote_account_for_askpass(cx);
        self.send_job(
            "fetch",
            Some("git fetch".into()),
            move |git_repo, mut cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            backend.identity(),
                            backend.is_trusted(),
                            askpass,
                            &mut cx,
                        );
                        let result = backend
                            .fetch(fetch_options, askpass, environment, cx.clone())
                            .await;
                        if result.is_ok() {
                            Self::refresh_branch_list(&this, backend, updates_tx, &mut cx).await?;
                        }
                        result
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            remote_identity,
                            remote_is_trusted,
                            askpass,
                            &mut cx,
                        );
                        let _askpass_operation =
                            RemoteAskPassOperation::new(askpass_id, askpass, askpass_delegates);

                        let response = client
                            .request(proto::Fetch {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                askpass_id,
                                remote: fetch_options.to_proto(),
                                unshallow: fetch_options == FetchOptions::Unshallow,
                            })
                            .await?;

                        Ok(RemoteCommandOutput {
                            stdout: response.stdout,
                            stderr: response.stderr,
                        })
                    }
                }
            },
        )
    }

    pub fn push(
        &mut self,
        branch: SharedString,
        remote_branch: SharedString,
        remote: SharedString,
        options: Option<PushOptions>,
        askpass: AskPassDelegate,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<PushOutput>> {
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let id = self.id;

        let args = options
            .map(|option| format!(" {}", option.flag()))
            .unwrap_or_default();

        let updates_tx = self
            .git_store()
            .and_then(|git_store| match &git_store.read(cx).state {
                GitStoreState::Local { downstream, .. } => downstream
                    .as_ref()
                    .map(|downstream| downstream.updates_tx.clone()),
                _ => None,
            });

        let this = cx.weak_entity();
        let (remote_identity, remote_is_trusted) = self.remote_account_for_askpass(cx);
        self.send_job(
            "push",
            Some(format!("git push {} {} {}:{}", args, remote, branch, remote_branch).into()),
            move |git_repo, mut cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            backend.identity(),
                            backend.is_trusted(),
                            askpass,
                            &mut cx,
                        );
                        let result = backend
                            .push(
                                branch.to_string(),
                                remote_branch.to_string(),
                                remote.to_string(),
                                options,
                                askpass,
                                environment.clone(),
                                cx.clone(),
                            )
                            .await;
                        // TODO would be nice to not have to do this manually
                        if result.is_ok() {
                            Self::refresh_branch_list(&this, backend, updates_tx, &mut cx).await?;
                        }
                        result.map(PushOutput::honoured)
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            remote_identity,
                            remote_is_trusted,
                            askpass,
                            &mut cx,
                        );
                        let _askpass_operation =
                            RemoteAskPassOperation::new(askpass_id, askpass, askpass_delegates);
                        let (proto_options, force_without_lease) = push_options_to_proto(options);
                        let response = client
                            .request(proto::Push {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                askpass_id,
                                branch_name: branch.to_string(),
                                remote_branch_name: remote_branch.to_string(),
                                remote_name: remote.to_string(),
                                options: proto_options,
                                force_without_lease,
                            })
                            .await?;

                        Ok(PushOutput {
                            kept_lease_against_request: push_kept_lease_against_request(
                                force_without_lease,
                                response.forced_without_lease,
                            ),
                            output: RemoteCommandOutput {
                                stdout: response.stdout,
                                stderr: response.stderr,
                            },
                        })
                    }
                }
            },
        )
    }

    pub fn pull(
        &mut self,
        branch: Option<SharedString>,
        remote: SharedString,
        rebase: bool,
        askpass: AskPassDelegate,
        cx: &mut App,
    ) -> oneshot::Receiver<Result<RemoteCommandOutput>> {
        let askpass_delegates = self.askpass_delegates.clone();
        let askpass_id = util::post_inc(&mut self.latest_askpass_id);
        let id = self.id;

        let mut status = "git pull".to_string();
        if rebase {
            status.push_str(" --rebase");
        }
        status.push_str(&format!(" {}", remote));
        if let Some(b) = &branch {
            status.push_str(&format!(" {}", b));
        }
        let (remote_identity, remote_is_trusted) = self.remote_account_for_askpass(cx);
        let account_applied = self.remote_git_account.applied.clone();

        self.send_job(
            "pull",
            Some(status.into()),
            move |git_repo, mut cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            backend.identity(),
                            backend.is_trusted(),
                            askpass,
                            &mut cx,
                        );
                        backend
                            .pull(
                                branch.as_ref().map(|b| b.to_string()),
                                remote.to_string(),
                                rebase,
                                askpass,
                                environment.clone(),
                                cx,
                            )
                            .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        // A pull may merge, making a commit as whoever the host's git is.
                        crate::git_accounts::ensure_remote_git_account_applied(account_applied)
                            .await?;
                        let askpass = crate::git_accounts::askpass_with_account_token(
                            remote_identity,
                            remote_is_trusted,
                            askpass,
                            &mut cx,
                        );
                        let _askpass_operation =
                            RemoteAskPassOperation::new(askpass_id, askpass, askpass_delegates);
                        let response = client
                            .request(proto::Pull {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                askpass_id,
                                rebase,
                                branch_name: branch.as_ref().map(|b| b.to_string()),
                                remote_name: remote.to_string(),
                            })
                            .await?;

                        Ok(RemoteCommandOutput {
                            stdout: response.stdout,
                            stderr: response.stderr,
                        })
                    }
                }
            },
        )
    }

    fn spawn_set_index_text_job(
        &mut self,
        path: RepoPath,
        content: Option<String>,
        encoding: &'static encoding_rs::Encoding,
        has_bom: bool,
        hunk_staging_operation_count: Option<usize>,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<anyhow::Result<()>> {
        let id = self.id;
        let this = cx.weak_entity();
        let git_store = self.git_store.clone();
        let abs_path = self.snapshot.repo_path_to_abs_path(&path);
        self.send_keyed_job(
            "spawn_set_index_text_job",
            Some(GitJobKey::WriteIndex(vec![path.clone()])),
            None,
            move |git_repo, mut cx| async move {
                log::debug!(
                    "start updating index text for buffer {}",
                    path.as_unix_str()
                );

                match git_repo {
                    RepositoryState::Local(LocalRepositoryState {
                        fs,
                        backend,
                        environment,
                        ..
                    }) => {
                        let executable = match fs.metadata(&abs_path).await {
                            Ok(Some(meta)) => meta.is_executable,
                            Ok(None) => false,
                            Err(_err) => false,
                        };
                        let content =
                            content.map(|content| encode_text(content, encoding, has_bom));
                        backend
                            .set_index_text(path.clone(), content, environment.clone(), executable)
                            .await?;
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::SetIndexText {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                path: path.as_unix_str().to_owned(),
                                text: content,
                            })
                            .await?;
                    }
                }
                log::debug!(
                    "finish updating index text for buffer {}",
                    path.as_unix_str()
                );

                if let Some(hunk_staging_operation_count) = hunk_staging_operation_count {
                    let project_path = this
                        .read_with(&cx, |this, cx| this.repo_path_to_project_path(&path, cx))
                        .ok()
                        .flatten();
                    git_store
                        .update(&mut cx, |git_store, cx| {
                            let buffer_id = git_store
                                .buffer_store
                                .read(cx)
                                .get_by_path(&project_path?)?
                                .read(cx)
                                .remote_id();
                            let diff_state = git_store.diffs.get(&buffer_id)?;
                            diff_state.update(cx, |diff_state, _| {
                                diff_state.hunk_staging_operation_count_as_of_write =
                                    hunk_staging_operation_count;
                            });
                            Some(())
                        })
                        .context("Git store dropped")?;
                }
                Ok(())
            },
        )
    }

    pub fn create_remote(
        &mut self,
        remote_name: String,
        remote_url: String,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "create_remote",
            Some(format!("git remote add {remote_name} {remote_url}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.create_remote(remote_name, remote_url).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitCreateRemote {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                remote_name,
                                remote_url,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn remove_remote(&mut self, remote_name: String) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "remove_remote",
            Some(format!("git remove remote {remote_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.remove_remote(remote_name).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitRemoveRemote {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                remote_name,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn get_remotes(
        &mut self,
        branch_name: Option<String>,
        is_push: bool,
    ) -> oneshot::Receiver<Result<Vec<Remote>>> {
        let id = self.id;
        self.send_job("get_remotes", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    let remote = if let Some(branch_name) = branch_name {
                        if is_push {
                            backend.get_push_remote(branch_name).await?
                        } else {
                            backend.get_branch_remote(branch_name).await?
                        }
                    } else {
                        None
                    };

                    match remote {
                        Some(remote) => Ok(vec![remote]),
                        None => backend.get_all_remotes().await,
                    }
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GetRemotes {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            branch_name,
                            is_push,
                        })
                        .await?;

                    let remotes = response
                        .remotes
                        .into_iter()
                        .map(|remotes| Remote {
                            name: remotes.name.into(),
                        })
                        .collect();

                    Ok(remotes)
                }
            }
        })
    }

    pub fn remote_urls(&mut self) -> oneshot::Receiver<Result<HashMap<String, String>>> {
        let id = self.id;
        self.send_job("remote_urls", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    Ok(backend.remote_urls().await)
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GetRemotes {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            branch_name: None,
                            is_push: false,
                        })
                        .await?;

                    Ok(response
                        .remotes
                        .into_iter()
                        .filter_map(|remote| Some((remote.name, remote.url?)))
                        .collect())
                }
            }
        })
    }

    pub fn branches(&mut self) -> oneshot::Receiver<Result<BranchesScanResult>> {
        let id = self.id;
        self.send_job("branches", None, move |repo, _| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.branches().await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitGetBranches {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        })
                        .await?;

                    let branches = response
                        .branches
                        .into_iter()
                        .map(|branch| proto_to_branch(&branch))
                        .collect();

                    Ok(BranchesScanResult {
                        branches,
                        error: response.error.map(SharedString::from),
                        ref_storage: RefStorageScan::default(),
                    })
                }
            }
        })
    }

    /// If this is a linked worktree (*NOT* the main checkout of a repository),
    /// returns the path for the linked worktree.
    ///
    /// Returns None if this is the main checkout.
    pub fn linked_worktree_path(&self) -> Option<&Arc<Path>> {
        self.snapshot
            .is_linked_worktree()
            .then_some(&self.work_directory_abs_path)
    }

    fn linked_worktree_anchor_path(&self) -> &Path {
        self.snapshot
            .main_worktree_abs_path()
            .or_else(|| repo_identity_path_if_local(&self.common_dir_abs_path, self.path_style))
            .unwrap_or(self.common_dir_abs_path.as_ref())
    }

    pub fn path_for_new_linked_worktree(
        &self,
        branch_name: &str,
        worktree_directory_setting: &str,
    ) -> Result<PathBuf> {
        let repository_anchor = self.linked_worktree_anchor_path();
        let project_name = repository_anchor
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("git repo must have a directory name"))?;
        let directory = worktrees_directory_for_repo(
            repository_anchor,
            worktree_directory_setting,
            self.path_style,
        )?;
        let directory = self.path_style.join_path(&directory, branch_name)?;
        self.path_style.join_path(&directory, project_name)
    }

    pub fn worktrees(&mut self) -> oneshot::Receiver<Result<Vec<GitWorktree>>> {
        let id = self.id;
        self.send_job("worktrees", None, move |repo, _| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.worktrees().await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitGetWorktrees {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        })
                        .await?;

                    let worktrees = response
                        .worktrees
                        .into_iter()
                        .map(|worktree| proto_to_worktree(&worktree))
                        .collect();

                    Ok(worktrees)
                }
            }
        })
    }

    pub fn create_worktree(
        &mut self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let job_description = match target.branch_name() {
            Some(branch_name) => format!("git worktree add: {branch_name}"),
            None => "git worktree add (detached)".to_string(),
        };
        self.send_job(
            "create_worktree",
            Some(job_description.into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.create_worktree(target, path).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let (name, commit, use_existing_branch) = match target {
                            CreateWorktreeTarget::ExistingBranch { branch_name } => {
                                (Some(branch_name), None, true)
                            }
                            CreateWorktreeTarget::NewBranch {
                                branch_name,
                                base_sha,
                            } => (Some(branch_name), base_sha, false),
                            CreateWorktreeTarget::Detached { base_sha } => (None, base_sha, false),
                        };

                        client
                            .request(proto::GitCreateWorktree {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                name: name.unwrap_or_default(),
                                directory: path.to_string_lossy().to_string(),
                                commit,
                                use_existing_branch,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    /// Returns the creation time of a linked worktree's git metadata
    /// directory. See [`GitRepository::worktree_created_at`]. For remote
    /// projects the stat runs on the remote host, where the worktree's
    /// filesystem lives.
    pub fn worktree_created_at(
        &mut self,
        worktree_path: PathBuf,
    ) -> oneshot::Receiver<Result<Option<SystemTime>>> {
        let id = self.id;
        self.send_job("worktree_created_at", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.worktree_created_at(worktree_path).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitWorktreeCreatedAt {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            worktree_path: worktree_path.to_string_lossy().to_string(),
                        })
                        .await?;
                    Ok(response.created_at.map(SystemTime::from))
                }
            }
        })
    }

    pub fn create_worktree_detached(
        &mut self,
        path: PathBuf,
        commit: String,
    ) -> oneshot::Receiver<Result<()>> {
        self.create_worktree(
            CreateWorktreeTarget::Detached {
                base_sha: Some(commit),
            },
            path,
        )
    }

    pub fn checkout_branch_in_worktree(
        &mut self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let description = if create {
            format!("git checkout -b {branch_name}")
        } else {
            format!("git checkout {branch_name}")
        };
        self.send_job(
            "checkout_branch_in_worktree",
            Some(description.into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend
                            .checkout_branch_in_worktree(branch_name, worktree_path, create)
                            .await
                    }
                    RepositoryState::Remote(_) => {
                        log::warn!(
                            "checkout_branch_in_worktree not supported for remote repositories"
                        );
                        Ok(())
                    }
                }
            },
        )
    }

    pub fn head_sha(&mut self) -> oneshot::Receiver<Result<Option<String>>> {
        let id = self.id;
        self.send_job("head_sha", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    Ok(backend.head_sha().await)
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitGetHeadSha {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        })
                        .await?;

                    Ok(response.sha)
                }
            }
        })
    }

    fn edit_ref(
        &mut self,
        ref_name: String,
        commit: Option<String>,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job("edit_ref", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => match commit {
                    Some(commit) => backend.update_ref(ref_name, commit).await,
                    None => backend.delete_ref(ref_name).await,
                },
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let action = match commit {
                        Some(sha) => proto::git_edit_ref::Action::UpdateToCommit(sha),
                        None => {
                            proto::git_edit_ref::Action::Delete(proto::git_edit_ref::DeleteRef {})
                        }
                    };
                    client
                        .request(proto::GitEditRef {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            ref_name,
                            action: Some(action),
                        })
                        .await?;
                    Ok(())
                }
            }
        })
    }

    pub fn update_ref(
        &mut self,
        ref_name: String,
        commit: String,
    ) -> oneshot::Receiver<Result<()>> {
        self.edit_ref(ref_name, Some(commit))
    }

    pub fn delete_ref(&mut self, ref_name: String) -> oneshot::Receiver<Result<()>> {
        self.edit_ref(ref_name, None)
    }

    pub fn repair_worktrees(&mut self) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job("repair_worktrees", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.repair_worktrees().await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    client
                        .request(proto::GitRepairWorktrees {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        })
                        .await?;
                    Ok(())
                }
            }
        })
    }

    pub fn create_archive_checkpoint(&mut self) -> oneshot::Receiver<Result<(String, String)>> {
        let id = self.id;
        self.send_job(
            "create_archive_checkpoint",
            None,
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.create_archive_checkpoint().await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let response = client
                            .request(proto::GitCreateArchiveCheckpoint {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                            })
                            .await?;
                        Ok((response.staged_commit_sha, response.unstaged_commit_sha))
                    }
                }
            },
        )
    }

    pub fn restore_archive_checkpoint(
        &mut self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "restore_archive_checkpoint",
            None,
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend
                            .restore_archive_checkpoint(staged_sha, unstaged_sha)
                            .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitRestoreArchiveCheckpoint {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                staged_commit_sha: staged_sha,
                                unstaged_commit_sha: unstaged_sha,
                            })
                            .await?;
                        Ok(())
                    }
                }
            },
        )
    }

    pub fn remove_worktree(&mut self, path: PathBuf, force: bool) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let repository_anchor_path: Arc<Path> = self.linked_worktree_anchor_path().into();
        self.send_job(
            "remove_worktree",
            Some(format!("git worktree remove: {}", path.display()).into()),
            move |repo, cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, fs, .. }) => {
                        // When forcing, delete the worktree directory ourselves before
                        // invoking git. `git worktree remove` can remove the admin
                        // metadata in `.git/worktrees/<name>` but fail to delete the
                        // working directory (it continues past directory-removal errors),
                        // leaving an orphaned folder on disk. Deleting first guarantees
                        // the directory is gone, and `git worktree remove --force`
                        // tolerates a missing working tree while cleaning up the admin
                        // entry. We keep this inside the `Local` arm so that for remote
                        // projects the deletion runs on the remote machine (where the
                        // `GitRemoveWorktree` RPC is handled against the local repo on
                        // the headless server) using its own filesystem.
                        //
                        // After a successful removal, also delete any empty ancestor
                        // directories between the worktree path and the configured
                        // base directory used when creating linked worktrees.
                        //
                        // Non-force removals are left untouched before git runs:
                        // `git worktree remove` must see the dirty working tree to
                        // refuse the operation.
                        if force {
                            fs.remove_dir(
                                &path,
                                RemoveOptions {
                                    recursive: true,
                                    ignore_if_not_exists: true,
                                },
                            )
                            .await
                            .with_context(|| {
                                format!("failed to delete worktree directory '{}'", path.display())
                            })?;
                        }

                        backend.remove_worktree(path.clone(), force).await?;

                        let managed_worktree_base = cx.update(|cx| {
                            let setting = &ProjectSettings::get_global(cx).git.worktree_directory;
                            worktrees_directory_for_repo(
                                &repository_anchor_path,
                                setting,
                                PathStyle::local(),
                            )
                            .log_err()
                        });

                        if let Some(managed_worktree_base) = managed_worktree_base {
                            remove_empty_managed_worktree_ancestors(
                                fs.as_ref(),
                                &path,
                                &managed_worktree_base,
                            )
                            .await;
                        }

                        Ok(())
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitRemoveWorktree {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                path: path.to_string_lossy().to_string(),
                                force,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn rename_worktree(
        &mut self,
        old_path: PathBuf,
        new_path: PathBuf,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "rename_worktree",
            Some(format!("git worktree move: {}", old_path.display()).into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.rename_worktree(old_path, new_path).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitRenameWorktree {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                old_path: old_path.to_string_lossy().to_string(),
                                new_path: new_path.to_string_lossy().to_string(),
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn default_branch(
        &mut self,
        include_remote_name: bool,
    ) -> oneshot::Receiver<Result<Option<SharedString>>> {
        let id = self.id;
        self.send_job("default_branch", None, move |repo, _| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.default_branch(include_remote_name).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GetDefaultBranch {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            include_remote_name,
                        })
                        .await?;

                    anyhow::Ok(response.branch.map(SharedString::from))
                }
            }
        })
    }

    pub fn diff_tree(
        &mut self,
        diff_type: DiffTreeType,
        _cx: &App,
    ) -> oneshot::Receiver<Result<TreeDiff>> {
        let repository_id = self.snapshot.id;
        self.send_job("diff_tree", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.diff_tree(diff_type).await
                }
                RepositoryState::Remote(RemoteRepositoryState { client, project_id }) => {
                    let (is_merge, includes_worktree, base, head) = match diff_type {
                        DiffTreeType::MergeBase { base, head } => (true, false, base, head),
                        // Older servers ignore `includes_worktree` and use the existing fields,
                        // so HEAD keeps this request valid as a committed-only fallback.
                        DiffTreeType::MergeBaseWithWorktree { base } => {
                            (true, true, base, "HEAD".into())
                        }
                        DiffTreeType::Since { base, head } => (false, false, base, head),
                    };
                    let response = client
                        .request(proto::GetTreeDiff {
                            project_id: project_id.0,
                            repository_id: repository_id.0,
                            is_merge,
                            base: base.to_string(),
                            head: head.to_string(),
                            includes_worktree,
                        })
                        .await?;

                    let entries = response
                        .entries
                        .into_iter()
                        .filter_map(|entry| {
                            let status = match entry.status() {
                                proto::tree_diff_status::Status::Added => TreeDiffStatus::Added,
                                proto::tree_diff_status::Status::Modified => {
                                    TreeDiffStatus::Modified {
                                        old: git::Oid::from_str(
                                            &entry.oid.context("missing oid").log_err()?,
                                        )
                                        .log_err()?,
                                    }
                                }
                                proto::tree_diff_status::Status::Deleted => {
                                    TreeDiffStatus::Deleted {
                                        old: git::Oid::from_str(
                                            &entry.oid.context("missing oid").log_err()?,
                                        )
                                        .log_err()?,
                                    }
                                }
                            };
                            Some((
                                RepoPath::from_rel_path(
                                    RelPath::from_unix_str(&entry.path).log_err()?,
                                ),
                                status,
                            ))
                        })
                        .collect();

                    Ok(TreeDiff { entries })
                }
            }
        })
    }

    pub fn diff(&mut self, diff_type: DiffType, _cx: &App) -> oneshot::Receiver<Result<String>> {
        let id = self.id;
        self.send_job("diff", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.diff(diff_type).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let (proto_diff_type, merge_base_ref) = match &diff_type {
                        DiffType::HeadToIndex => {
                            (proto::git_diff::DiffType::HeadToIndex.into(), None)
                        }
                        DiffType::HeadToWorktree => {
                            (proto::git_diff::DiffType::HeadToWorktree.into(), None)
                        }
                        DiffType::MergeBase { base_ref } => (
                            proto::git_diff::DiffType::MergeBase.into(),
                            Some(base_ref.to_string()),
                        ),
                    };
                    let response = client
                        .request(proto::GitDiff {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            diff_type: proto_diff_type,
                            merge_base_ref,
                        })
                        .await?;

                    Ok(response.diff)
                }
            }
        })
    }

    /// Runs a single git subcommand in this repository (locally, or on the host
    /// of a remote project) and returns its raw output.
    ///
    /// The subcommand is validated against [`git::repository::ALLOWED_GIT_COMMANDS`]
    /// where it runs. Prefer the typed wrappers below over calling this directly.
    pub fn run_git_command(
        &mut self,
        args: Vec<String>,
        stdin: Option<String>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_git_command_with_env(args, stdin, HashMap::default())
    }

    /// [`Self::run_git_command`] with extra environment variables, checked against
    /// [`git::repository::ALLOWED_GIT_COMMAND_ENV`] where the command runs. Only `commit-tree`
    /// needs this, to keep a rewritten commit's original author.
    pub fn run_git_command_with_env(
        &mut self,
        args: Vec<String>,
        stdin: Option<String>,
        command_env: HashMap<String, String>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let status = SharedString::from(format!("git {}", args.join(" ")));
        let account_applied = crate::git_accounts::command_makes_commits(&args)
            .then(|| self.remote_git_account.applied.clone());
        self.send_job(
            "run_git_command",
            Some(status),
            move |repo, _cx| async move {
                if let Some(account_applied) = account_applied {
                    crate::git_accounts::ensure_remote_git_account_applied(account_applied).await?;
                }
                Self::run_command_in_job(&repo, id, args, stdin, command_env).await
            },
        )
    }

    /// [`Self::run_git_command_with_env`] with every hook turned off, through
    /// [`git::repository::GitRepository::run_command_without_hooks`] wherever the repository is:
    /// how an AI agent's git commands run (ADR-F90). A remote host that cannot say it turned
    /// hooks off gets an error instead of the command.
    pub fn run_git_command_without_hooks(
        &mut self,
        args: Vec<String>,
        stdin: Option<String>,
        command_env: HashMap<String, String>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let status = SharedString::from(format!("git {}", args.join(" ")));
        let account_applied = crate::git_accounts::command_makes_commits(&args)
            .then(|| self.remote_git_account.applied.clone());
        self.send_job(
            "run_git_command",
            Some(status),
            move |repo, _cx| async move {
                if let Some(account_applied) = account_applied {
                    crate::git_accounts::ensure_remote_git_account_applied(account_applied).await?;
                }
                match repo {
                    RepositoryState::Local(LocalRepositoryState {
                        backend,
                        environment,
                        ..
                    }) => {
                        backend
                            .run_command_without_hooks(
                                args,
                                stdin,
                                environment.clone(),
                                command_env,
                            )
                            .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let mut env = command_env;
                        env.insert(GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY.to_string(), String::new());
                        let response = client
                            .request(proto::GitRunCommand {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                args,
                                stdin,
                                env: env.into_iter().collect(),
                            })
                            .await
                            .map_err(remote_hooks_refusal)?;
                        remote_output_without_hooks(response)
                    }
                }
            },
        )
    }

    /// One git subcommand against a repository already in hand inside a job.
    ///
    /// The multi-step wrappers below each have to be a *single* job: [`Self::run_git_command`]
    /// queues a new one, and a running job waiting on a job queued behind it never finishes.
    async fn run_command_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        args: Vec<String>,
        stdin: Option<String>,
        command_env: HashMap<String, String>,
    ) -> Result<GitCommandOutput> {
        match repo {
            RepositoryState::Local(LocalRepositoryState {
                backend,
                environment,
                ..
            }) => {
                backend
                    .run_command(args, stdin, environment.clone(), command_env)
                    .await
            }
            RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                let response = client
                    .request(proto::GitRunCommand {
                        project_id: project_id.0,
                        repository_id: id.to_proto(),
                        args,
                        stdin,
                        env: command_env.into_iter().collect(),
                    })
                    .await?;

                Ok(GitCommandOutput {
                    stdout: response.stdout,
                    stderr: response.stderr,
                    exit_code: response.exit_code,
                })
            }
        }
    }

    /// [`Self::run_command_in_job`] for a command with no stdin and no extra environment.
    async fn run_step_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        args: Vec<String>,
    ) -> Result<GitCommandOutput> {
        Self::run_command_in_job(repo, id, args, None, HashMap::default()).await
    }

    /// Runs `steps` (never empty) in order and stops at the first command git refuses, handing
    /// back that command's output so the user is told which step actually failed.
    async fn run_steps_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        steps: Vec<Vec<String>>,
    ) -> Result<GitCommandOutput> {
        let mut last = GitCommandOutput::default();
        for args in steps {
            last = Self::run_step_in_job(repo, id, args).await?;
            if !last.succeeded() {
                break;
            }
        }
        Ok(last)
    }

    /// `git merge <committish>`. Conflicts come back as a successful future with
    /// a non-zero [`GitCommandOutput::exit_code`], not as an error.
    pub fn merge(
        &mut self,
        committish: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver = self.run_git_command(vec!["merge".into(), committish], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git reset --hard <committish>`. Discards uncommitted work; callers must
    /// confirm with the user first.
    pub fn reset_hard(
        &mut self,
        committish: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver =
            self.run_git_command(vec!["reset".into(), "--hard".into(), committish], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Produces the text of a patch file without touching the working tree.
    pub fn create_patch(
        &mut self,
        source: PatchSource,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_git_command(source.args(), None)
    }

    /// `git apply` of a patch produced elsewhere. `contents` is streamed to the
    /// repository host over stdin so this works for remote projects, where the
    /// chosen patch file may not exist on the host at all.
    pub fn apply_patch(
        &mut self,
        contents: String,
        three_way: bool,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let mut args = vec!["apply".to_string()];
        if three_way {
            args.push("--3way".into());
        }
        // No file argument: `git apply` reads the patch from stdin.
        let receiver = self.run_git_command(args, Some(contents));
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git cherry-pick [-n] <sha>…`, oldest first so the commits land in their original order.
    pub fn cherry_pick(
        &mut self,
        shas_oldest_first: Vec<String>,
        no_commit: bool,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let mut args = vec!["cherry-pick".to_string()];
        if no_commit {
            args.push("-n".into());
        }
        args.extend(shas_oldest_first);
        let receiver = self.run_git_command(args, None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git revert [-n] <sha>…`, newest first so each revert applies to the tree the one before
    /// it left behind.
    pub fn revert_commits(
        &mut self,
        shas_newest_first: Vec<String>,
        no_commit: bool,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let mut args = vec!["revert".to_string()];
        if no_commit {
            args.push("-n".into());
        }
        args.extend(shas_newest_first);
        let receiver = self.run_git_command(args, None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git reset --soft|--mixed|--hard <committish>`. Callers must confirm `Hard` with the user:
    /// it throws uncommitted work away.
    pub fn reset_to(
        &mut self,
        committish: String,
        mode: ResetMode,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let mode_flag = match mode {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        };
        let receiver =
            self.run_git_command(vec!["reset".into(), mode_flag.into(), committish], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git checkout --detach <committish>`, leaving HEAD on the commit itself.
    pub fn checkout_detached(
        &mut self,
        committish: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver =
            self.run_git_command(vec!["checkout".into(), "--detach".into(), committish], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git tag <name> <committish>`, annotated (`-a -m`) when a message is given.
    pub fn create_tag(
        &mut self,
        name: String,
        committish: String,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let mut args = vec!["tag".to_string()];
        if let Some(message) = message {
            args.extend(["-a".to_string(), "-m".to_string(), message]);
        }
        args.extend([name, committish]);
        let receiver = self.run_git_command(args, None);
        // A tag is a ref, not a worktree change, but the refs the panel shows come from the same
        // scan.
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git tag -d <name>`.
    pub fn delete_tag(
        &mut self,
        name: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver = self.run_git_command(vec!["tag".into(), "-d".into(), name], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// `git pack-refs --all`: folds the loose ref files into `packed-refs`. The refs themselves
    /// stay as they are; only the way git stores them changes.
    pub fn pack_refs(
        &mut self,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver = self.run_git_command(vec!["pack-refs".into(), "--all".into()], None);
        // The loose ref count shown to the user comes from the ref scan.
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Every tag in the repository, newest first.
    pub fn tags(&mut self) -> oneshot::Receiver<Result<Vec<TagInfo>>> {
        self.tags_job(None)
    }

    /// [`Self::tags`] for a view that only needs its newest request answered: a request of the
    /// same `requester` still waiting in the queue is dropped (its receiver is cancelled).
    pub fn latest_tags(&mut self, requester: EntityId) -> oneshot::Receiver<Result<Vec<TagInfo>>> {
        self.tags_job(Some(GitJobKey::LatestTags(requester)))
    }

    fn tags_job(&mut self, key: Option<GitJobKey>) -> oneshot::Receiver<Result<Vec<TagInfo>>> {
        let id = self.id;
        self.send_keyed_job("tags", key, None, move |repo, cx| async move {
            let output = Self::run_step_in_job(
                &repo,
                id,
                vec![
                    "for-each-ref".into(),
                    "--sort=-creatordate".into(),
                    format!("--format={TAG_LIST_FORMAT}"),
                    "refs/tags/".into(),
                ],
            )
            .await?;
            anyhow::ensure!(
                output.succeeded(),
                i18n::t!(
                    "Could not get the list of tags: {error}",
                    error = output.failure_message()
                )
            );
            // Tens of thousands of tags take long enough to parse to stall a frame.
            Ok(cx
                .background_spawn(async move { TagInfo::parse_list(&output.stdout) })
                .await)
        })
    }

    /// `git describe --tags --long <committish>`, or `None` when no tag reaches the commit.
    pub fn describe(
        &mut self,
        committish: String,
    ) -> oneshot::Receiver<Result<Option<TagDistance>>> {
        self.describe_job(committish, None)
    }

    /// [`Self::describe`] for a view that follows its selection: a request of the same
    /// `requester` still waiting in the queue is dropped (its receiver is cancelled), so moving
    /// the selection quickly does not queue a `git describe` per commit passed over.
    pub fn latest_describe(
        &mut self,
        committish: String,
        requester: EntityId,
    ) -> oneshot::Receiver<Result<Option<TagDistance>>> {
        self.describe_job(committish, Some(GitJobKey::LatestDescribe(requester)))
    }

    fn describe_job(
        &mut self,
        committish: String,
        key: Option<GitJobKey>,
    ) -> oneshot::Receiver<Result<Option<TagDistance>>> {
        let id = self.id;
        self.send_keyed_job("describe", key, None, move |repo, _cx| async move {
            let output = Self::run_step_in_job(
                &repo,
                id,
                vec![
                    "describe".into(),
                    "--tags".into(),
                    "--long".into(),
                    committish,
                ],
            )
            .await?;
            // "No names found" is an ordinary answer for a repository without tags, not a failure.
            if !output.succeeded() {
                return Ok(None);
            }
            let described = output.stdout.trim();
            TagDistance::parse(described).map(Some).with_context(|| {
                i18n::t!("Could not interpret the output of git describe: {described}")
            })
        })
    }

    /// Whether a `cherry-pick` / `revert` / `rebase` is stopped part-way, and whether it stopped
    /// on a conflict.
    ///
    /// The git panel asks after every scan, so this is keyed: a burst of scans must not queue a
    /// burst of `rev-parse` processes ahead of the work the user is waiting for. The caller whose
    /// probe gets dropped sees its receiver close and keeps the state it already has.
    pub fn sequencer_state(&mut self) -> oneshot::Receiver<Result<Option<SequencerState>>> {
        let id = self.id;
        self.send_keyed_job(
            "sequencer_state",
            Some(GitJobKey::SequencerState),
            None,
            move |repo, _cx| async move {
                let verify_ref = async |ref_name: &str| {
                    Self::run_step_in_job(
                        &repo,
                        id,
                        vec![
                            "rev-parse".into(),
                            "--verify".into(),
                            "--quiet".into(),
                            ref_name.to_string(),
                        ],
                    )
                    .await
                    .map(|probe| probe.succeeded())
                };

                let mut in_progress = None;
                for kind in SequencerKind::all() {
                    if verify_ref(kind.in_progress_ref()).await? {
                        in_progress = Some(kind);
                        break;
                    }
                }

                // The index rather than the last scan: the banner has to be right the moment a
                // cherry-pick comes back, which is before the scan it schedules has finished.
                let unmerged = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "diff".into(),
                        "--name-only".into(),
                        "--diff-filter=U".into(),
                    ],
                )
                .await?;
                let unmerged_paths = !unmerged.stdout.trim().is_empty();

                // Only asked when there are conflicts no sequencer ref accounts for, which keeps
                // these off the path every scan of an ordinary repository takes.
                let mut merge_in_progress = false;
                let mut head_is_detached = false;
                if in_progress.is_none() && unmerged_paths {
                    merge_in_progress = verify_ref("MERGE_HEAD").await?;
                    if !merge_in_progress {
                        let head = Self::run_step_in_job(
                            &repo,
                            id,
                            vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
                        )
                        .await?;
                        // A repository without a first commit prints `HEAD` too, on its way to
                        // failing, so the exit status has to agree that this is a real answer.
                        head_is_detached = head.succeeded() && head.stdout.trim() == "HEAD";
                    }
                }

                Ok(sequencer_state_from_probes(SequencerProbes {
                    in_progress,
                    unmerged_paths,
                    merge_in_progress,
                    head_is_detached,
                }))
            },
        )
    }

    /// `git <cherry-pick|revert|rebase> --continue`.
    pub fn sequencer_continue(
        &mut self,
        kind: SequencerKind,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_sequencer_step(kind, "--continue", cx)
    }

    /// `git <cherry-pick|revert|rebase> --abort`.
    pub fn sequencer_abort(
        &mut self,
        kind: SequencerKind,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_sequencer_step(kind, "--abort", cx)
    }

    /// `git <cherry-pick|revert|rebase> --skip`.
    pub fn sequencer_skip(
        &mut self,
        kind: SequencerKind,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_sequencer_step(kind, "--skip", cx)
    }

    fn run_sequencer_step(
        &mut self,
        kind: SequencerKind,
        flag: &'static str,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let receiver =
            self.run_git_command(vec![kind.subcommand().to_string(), flag.to_string()], None);
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Rewrites an earlier commit's message. The commit and everything after it get new SHAs, so
    /// a branch that was already pushed needs a force push afterwards.
    ///
    /// `git rebase -i` is not an option: it drives a todo list through an editor, and this fork
    /// never lets git start a program (see `ALLOWED_GIT_COMMAND_ENV`). The commit is rebuilt with
    /// `commit-tree` instead and the branch is moved onto it with `rebase --onto`.
    pub fn reword_commit(
        &mut self,
        sha: String,
        message: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let branch_name = self.current_branch_name();
        let receiver = self.send_job(
            "reword_commit",
            Some(format!("git commit-tree {sha}").into()),
            move |repo, _cx| async move {
                let head = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        "HEAD".into(),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    head.succeeded(),
                    i18n::t!(
                        "Could not resolve HEAD: {error}",
                        error = head.failure_message()
                    )
                );
                if head.stdout.trim() == sha {
                    // `--only` with no pathspec keeps whatever is staged out of the amended
                    // commit, so rewording never smuggles the user's staged work into history.
                    return Self::run_step_in_job(
                        &repo,
                        id,
                        vec![
                            "commit".into(),
                            "--amend".into(),
                            "--only".into(),
                            "-m".into(),
                            message,
                        ],
                    )
                    .await;
                }

                Self::ensure_commit_is_in_head_in_job(&repo, id, &sha).await?;
                Self::ensure_range_is_linear_in_job(&repo, id, format!("{sha}..HEAD")).await?;
                Self::ensure_no_ignored_file_is_overwritten_in_job(
                    &repo,
                    id,
                    git::repository::RewrittenCommits::Range(&format!("{sha}..HEAD")),
                )
                .await?;
                let author = Self::author_env_in_job(&repo, id, &sha).await?;
                let parent = Self::parent_of_in_job(&repo, id, &sha).await?;
                let rebuilt = Self::commit_tree_in_job(
                    &repo,
                    id,
                    format!("{sha}^{{tree}}"),
                    parent,
                    message,
                    author,
                )
                .await?;
                let new_sha = match rebuilt {
                    Ok(new_sha) => new_sha,
                    Err(output) => return Ok(output),
                };

                let stash_before = Self::stash_top_in_job(&repo, id).await?;
                let rebased = Self::run_step_in_job(
                    &repo,
                    id,
                    git::repository::rebase_onto_args(&new_sha, &sha, branch_name.as_deref()),
                )
                .await?;
                if rebased.succeeded() && Self::stash_top_in_job(&repo, id).await? != stash_before {
                    return Err(ChangesLeftInStash.into());
                }
                Ok(rebased)
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Squashes the consecutive range `oldest..=newest` on the current branch into one commit,
    /// with the same plumbing [`Self::reword_commit`] uses: the new commit keeps `newest`'s tree
    /// and hangs off `oldest`'s parent.
    pub fn squash_range(
        &mut self,
        oldest_sha: String,
        newest_sha: String,
        message: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let branch_name = self.current_branch_name();
        let receiver = self.send_job(
            "squash_range",
            Some(format!("git commit-tree {newest_sha}").into()),
            move |repo, _cx| async move {
                // Both ends matter: `newest` decides what the final `rebase --onto` replays, and
                // an off-branch `oldest` would hang the branch off that commit's parent, dropping
                // everything between it and `newest` from the branch.
                Self::ensure_commit_is_in_head_in_job(&repo, id, &newest_sha).await?;
                if oldest_sha != newest_sha {
                    Self::ensure_commit_is_in_head_in_job(&repo, id, &oldest_sha).await?;
                }
                let parent = Self::parent_of_in_job(&repo, id, &oldest_sha).await?;
                let range = match &parent {
                    Some(parent) => format!("{parent}..{newest_sha}"),
                    // `oldest` is the repository's first commit, so the range is all of history
                    // up to `newest`.
                    None => newest_sha.clone(),
                };
                Self::ensure_range_is_linear_in_job(&repo, id, range).await?;
                // The squashed commit takes `newest`'s tree, so only what comes after it is
                // checked out or replayed.
                Self::ensure_no_ignored_file_is_overwritten_in_job(
                    &repo,
                    id,
                    git::repository::RewrittenCommits::Range(&format!("{newest_sha}..HEAD")),
                )
                .await?;
                let author = Self::author_env_in_job(&repo, id, &oldest_sha).await?;
                let rebuilt = Self::commit_tree_in_job(
                    &repo,
                    id,
                    format!("{newest_sha}^{{tree}}"),
                    parent,
                    message,
                    author,
                )
                .await?;
                let new_sha = match rebuilt {
                    Ok(new_sha) => new_sha,
                    Err(output) => return Ok(output),
                };

                let stash_before = Self::stash_top_in_job(&repo, id).await?;
                let rebased = Self::run_step_in_job(
                    &repo,
                    id,
                    git::repository::rebase_onto_args(
                        &new_sha,
                        &newest_sha,
                        branch_name.as_deref(),
                    ),
                )
                .await?;
                if rebased.succeeded() && Self::stash_top_in_job(&repo, id).await? != stash_before {
                    return Err(ChangesLeftInStash.into());
                }
                Ok(rebased)
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Builds a merge-request branch out of the merge-request box: one commit on top of
    /// `build.base` carrying the changes of every commit in the box, without ever moving the
    /// branch the user is on.
    ///
    /// Everything happens on `build.scratch_branch` (see
    /// [`git::repository::merge_request_pick_commands`] and
    /// [`git::repository::merge_request_finish_commands`]), and a build that works puts the user
    /// back on `build.origin_branch`. A conflict leaves them on the scratch branch with the
    /// cherry-pick stopped, which the git panel's banner continues or aborts; once continued,
    /// [`Self::finish_merge_request_branch`] does the rest. Any other refusal puts everything back
    /// the way it was.
    pub fn build_merge_request_branch(
        &mut self,
        build: MergeRequestBuild,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<MergeRequestBuildOutcome>> {
        let id = self.id;
        let receiver = self.send_job(
            "build_merge_request_branch",
            Some(format!("git switch -c {}", build.scratch_branch).into()),
            move |repo, _cx| async move {
                // Nothing has moved until the scratch branch is created, so every refusal up to
                // there is a plain "no" that leaves nothing behind.
                let prepared = async {
                    Self::ensure_head_is_branch_in_job(&repo, id, &build.origin_branch).await?;
                    Self::ensure_nothing_in_progress_in_job(&repo, id).await?;
                    let dirty = Self::run_step_in_job(
                        &repo,
                        id,
                        vec!["diff".into(), "--quiet".into(), "HEAD".into()],
                    )
                    .await?;
                    anyhow::ensure!(
                        dirty.succeeded(),
                        i18n::t!(
                            "There are uncommitted changes. Commit or stash them before creating the MR branch."
                        )
                    );
                    anyhow::ensure!(
                        Self::verify_ref_in_job(
                            &repo,
                            id,
                            &format!("refs/heads/{}", build.scratch_branch)
                        )
                        .await?
                        .is_none(),
                        i18n::t!(
                            "The scratch branch \"{scratch_branch}\" of the previous attempt is still there. Check its contents and delete it, then try again.",
                            scratch_branch = build.scratch_branch
                        )
                    );
                    let base_sha =
                        Self::verify_ref_in_job(&repo, id, &format!("{}^{{commit}}", build.base))
                            .await?
                            .with_context(|| {
                                i18n::t!("The base \"{base}\" was not found.", base = build.base)
                            })?;
                    // Recreating the base itself would move it onto the merge request's commit.
                    anyhow::ensure!(
                        build.merge_request_branch != build.base
                            && build.base != format!("refs/heads/{}", build.merge_request_branch),
                        i18n::t!(
                            "The base branch \"{merge_request_branch}\" cannot be the MR branch. Use another name.",
                            merge_request_branch = build.merge_request_branch
                        )
                    );
                    let previous_sha = Self::merge_request_previous_sha_in_job(
                        &repo,
                        id,
                        &build.merge_request_branch,
                        build.recreate,
                    )
                    .await?;
                    // The switch onto the base refuses to overwrite an ignored file by itself
                    // (`--no-overwrite-ignore`); the cherry-picks after it have no such option.
                    Self::ensure_no_ignored_file_is_overwritten_in_job(
                        &repo,
                        id,
                        git::repository::RewrittenCommits::Commits(&build.shas_oldest_first),
                    )
                    .await?;
                    // Both halves are built before anything runs, so that a name or a message
                    // the finish would refuse is refused while the user is still on their own
                    // branch.
                    let picks = git::repository::merge_request_pick_commands(
                        &base_sha,
                        &build.shas_oldest_first,
                        &build.scratch_branch,
                    )?;
                    let finish = git::repository::merge_request_finish_commands(
                        &base_sha,
                        &build.message,
                        &build.merge_request_branch,
                        build.recreate,
                        &build.origin_branch,
                        &build.scratch_branch,
                    )?;
                    let mut picks = picks.into_iter();
                    let (Some(create_scratch), Some(cherry_pick)) = (picks.next(), picks.next())
                    else {
                        anyhow::bail!(i18n::t!("Could not prepare the commands that build the MR branch."));
                    };
                    anyhow::Ok((base_sha, previous_sha, create_scratch, cherry_pick, finish))
                }
                .await;
                let (base_sha, previous_sha, create_scratch, cherry_pick, finish) = match prepared
                {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return Ok(MergeRequestBuildOutcome::Refused {
                            reason: format!("{error:#}"),
                            left_on_scratch_branch: false,
                        });
                    }
                };

                let created = Self::run_step_in_job(&repo, id, create_scratch).await?;
                if !created.succeeded() {
                    // A failing `post-checkout` hook fails the switch after it has happened.
                    let on_scratch_branch =
                        Self::ensure_head_is_branch_in_job(&repo, id, &build.scratch_branch)
                            .await
                            .is_ok();
                    let left_on_scratch_branch = on_scratch_branch
                        && !Self::leave_merge_request_scratch_in_job(
                            &repo,
                            id,
                            &build.origin_branch,
                            &build.scratch_branch,
                        )
                        .await;
                    return Ok(MergeRequestBuildOutcome::Refused {
                        reason: created.failure_message(),
                        left_on_scratch_branch,
                    });
                }
                let picked = Self::run_step_in_job(&repo, id, cherry_pick).await?;
                if !picked.succeeded() {
                    if Self::verify_ref_in_job(&repo, id, "CHERRY_PICK_HEAD")
                        .await?
                        .is_some()
                    {
                        return Ok(MergeRequestBuildOutcome::Stopped {
                            base_sha,
                            reason: picked.failure_message(),
                        });
                    }
                    // Refused for some other reason than a conflict (a commit git cannot pick,
                    // say). A pick of several commits can still have left its todo list behind,
                    // which would make every later cherry-pick in this repository refuse to
                    // start, so it goes before the scratch branch does.
                    let aborted = Self::run_step_in_job(
                        &repo,
                        id,
                        vec!["cherry-pick".into(), "--abort".into()],
                    )
                    .await;
                    if let Err(error) = aborted {
                        log::warn!("could not clean up the stopped cherry-pick: {error:#}");
                    }
                    let left_on_scratch_branch = !Self::leave_merge_request_scratch_in_job(
                        &repo,
                        id,
                        &build.origin_branch,
                        &build.scratch_branch,
                    )
                    .await;
                    return Ok(MergeRequestBuildOutcome::Refused {
                        reason: picked.failure_message(),
                        left_on_scratch_branch,
                    });
                }
                Self::finish_merge_request_in_job(&repo, id, finish, previous_sha, true).await
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// The rest of a merge-request build that stopped on a conflict and was continued from the
    /// git panel's banner: the replayed commits on the scratch branch are folded into one, the
    /// merge-request branch is pointed at it, and the user is put back where they started.
    ///
    /// Unlike a fresh build, a refusal here leaves the user on the scratch branch: the commits on
    /// it carry the conflict resolution they did by hand.
    pub fn finish_merge_request_branch(
        &mut self,
        continuation: MergeRequestContinuation,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<MergeRequestBuildOutcome>> {
        let id = self.id;
        let receiver = self.send_job(
            "finish_merge_request_branch",
            Some(format!("git switch {}", continuation.origin_branch).into()),
            move |repo, _cx| async move {
                Self::ensure_head_is_branch_in_job(&repo, id, &continuation.scratch_branch)
                    .await?;
                Self::ensure_nothing_in_progress_in_job(&repo, id).await?;
                // Staged changes are allowed: they are what a fold that got as far as
                // `reset --soft` before being refused left behind.
                let unstaged = Self::run_step_in_job(
                    &repo,
                    id,
                    vec!["diff".into(), "--quiet".into()],
                )
                .await?;
                anyhow::ensure!(
                    unstaged.succeeded(),
                    i18n::t!(
                        "The scratch branch \"{scratch_branch}\" has unstaged changes. Stage or discard them, then try again.",
                        scratch_branch = continuation.scratch_branch
                    )
                );
                let off_base = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "log".into(),
                        "--format=%H".into(),
                        format!("HEAD..{}", continuation.base_sha),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    off_base.succeeded() && off_base.stdout.trim().is_empty(),
                    i18n::t!(
                        "The scratch branch \"{scratch_branch}\" does not start at the base, so it cannot go into the MR branch.",
                        scratch_branch = continuation.scratch_branch
                    )
                );
                let previous_sha = Self::merge_request_previous_sha_in_job(
                    &repo,
                    id,
                    &continuation.merge_request_branch,
                    continuation.recreate,
                )
                .await?;
                let finish = git::repository::merge_request_finish_commands(
                    &continuation.base_sha,
                    &continuation.message,
                    &continuation.merge_request_branch,
                    continuation.recreate,
                    &continuation.origin_branch,
                    &continuation.scratch_branch,
                )?;
                Self::finish_merge_request_in_job(&repo, id, finish, previous_sha, false).await
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Runs [`git::repository::MergeRequestFinishCommands`]. `clean_up_on_refusal` throws the
    /// scratch branch away when git refuses to fold or point, which only a fresh build may do.
    async fn finish_merge_request_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        finish: git::repository::MergeRequestFinishCommands,
        previous_sha: Option<String>,
        clean_up_on_refusal: bool,
    ) -> Result<MergeRequestBuildOutcome> {
        let git::repository::MergeRequestFinishCommands {
            fold_and_point,
            return_to_origin,
            base_sha,
            merge_request_branch,
            origin_branch,
            scratch_branch,
        } = finish;
        let refuse = async |reason: String| -> Result<MergeRequestBuildOutcome> {
            let left_on_scratch_branch = if clean_up_on_refusal {
                // Whatever the fold left in the index and the working tree is the replayed
                // commits and nothing else (the build refused to start on a dirty tree), and
                // `switch` would carry it over onto the user's branch.
                let reset = Self::run_step_in_job(
                    repo,
                    id,
                    vec!["reset".into(), "--hard".into(), "HEAD".into()],
                )
                .await?;
                !reset.succeeded()
                    || !Self::leave_merge_request_scratch_in_job(
                        repo,
                        id,
                        &origin_branch,
                        &scratch_branch,
                    )
                    .await
            } else {
                true
            };
            Ok(MergeRequestBuildOutcome::Refused {
                reason,
                left_on_scratch_branch,
            })
        };

        // The index rather than HEAD: a continuation may find the replayed commits already
        // folded into the index by an earlier attempt that got as far as `reset --soft`.
        let unchanged = Self::run_step_in_job(
            repo,
            id,
            vec![
                "diff".into(),
                "--cached".into(),
                "--quiet".into(),
                base_sha.clone(),
            ],
        )
        .await?;
        if unchanged.succeeded() {
            return refuse(
                i18n::t!(
                    "The base already has every change of the commits in the MR box. There is no difference to make an MR of."
                )
                .to_string(),
            )
            .await;
        }
        let folded = Self::run_steps_in_job(repo, id, fold_and_point).await?;
        if !folded.succeeded() {
            return refuse(folded.failure_message()).await;
        }

        // HEAD is the commit `branch` just pointed the merge-request branch at.
        let merge_request_sha = Self::verify_ref_in_job(repo, id, "HEAD")
            .await?
            .with_context(|| {
                i18n::t!("Could not check the created MR branch \"{merge_request_branch}\".")
            })?;
        let returned = Self::run_steps_in_job(repo, id, return_to_origin).await?;
        Ok(MergeRequestBuildOutcome::Created {
            merge_request_sha,
            previous_sha,
            return_failure: (!returned.succeeded()).then(|| returned.failure_message()),
        })
    }

    /// Back to `origin_branch` with the scratch branch deleted; `false` when git refused either.
    async fn leave_merge_request_scratch_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        origin_branch: &str,
        scratch_branch: &str,
    ) -> bool {
        let steps = match git::repository::rewrite_cleanup_commands(origin_branch, scratch_branch) {
            Ok(steps) => steps,
            Err(error) => {
                log::warn!("could not go back from the scratch branch: {error:#}");
                return false;
            }
        };
        match Self::run_steps_in_job(repo, id, steps).await {
            Ok(output) => output.succeeded(),
            Err(error) => {
                log::warn!("could not go back from the scratch branch: {error:#}");
                false
            }
        }
    }

    /// `git rev-parse --verify --quiet <name>`: the commit `name` points at, or `None` when there
    /// is no such ref.
    async fn verify_ref_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        name: &str,
    ) -> Result<Option<String>> {
        let output = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                name.to_string(),
            ],
        )
        .await?;
        Ok(output
            .succeeded()
            .then(|| output.stdout.trim().to_string())
            .filter(|sha| !sha.is_empty()))
    }

    /// Refuses unless `branch` is what is checked out. The caller read the branch from a snapshot
    /// that may be a scan behind, and remembered it as the way back before calling.
    async fn ensure_head_is_branch_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        branch: &str,
    ) -> Result<()> {
        let head = Self::run_step_in_job(
            repo,
            id,
            vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        )
        .await?;
        anyhow::ensure!(
            head.succeeded(),
            i18n::t!(
                "Could not check the current branch: {error}",
                error = head.failure_message()
            )
        );
        let current = head.stdout.trim();
        anyhow::ensure!(
            current != "HEAD",
            i18n::t!("Cannot create an MR branch on a detached HEAD. Switch to a branch first.")
        );
        anyhow::ensure!(
            current == branch,
            i18n::t!(
                "The checked-out branch has changed to \"{current}\" (expected \"{branch}\"). Try again."
            )
        );
        Ok(())
    }

    /// Refuses while a cherry-pick, revert, rebase or merge is stopped part-way, or while the index
    /// still has conflicts.
    async fn ensure_nothing_in_progress_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
    ) -> Result<()> {
        for kind in SequencerKind::all() {
            anyhow::ensure!(
                Self::verify_ref_in_job(repo, id, kind.in_progress_ref())
                    .await?
                    .is_none(),
                i18n::t!(
                    "The {kind} stopped partway. Continue or abort it in the Git panel's banner first.",
                    kind = kind.label()
                )
            );
        }
        anyhow::ensure!(
            Self::verify_ref_in_job(repo, id, "MERGE_HEAD")
                .await?
                .is_none(),
            i18n::t!("A merge stopped partway. Finish or abort the merge first.")
        );
        let unmerged = Self::run_step_in_job(
            repo,
            id,
            vec![
                "diff".into(),
                "--name-only".into(),
                "--diff-filter=U".into(),
            ],
        )
        .await?;
        anyhow::ensure!(
            unmerged.succeeded() && unmerged.stdout.trim().is_empty(),
            i18n::t!("Some files have unresolved conflicts. Resolve them first.")
        );
        Ok(())
    }

    /// The local name of the repository's default branch (`main` for `origin/main`), as
    /// [`Self::default_branch`] finds it; `None` when it cannot be told.
    async fn local_default_branch_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
    ) -> Option<SharedString> {
        let default_branch = match repo {
            RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                backend.default_branch(false).await
            }
            RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => client
                .request(proto::GetDefaultBranch {
                    project_id: project_id.0,
                    repository_id: id.to_proto(),
                    include_remote_name: false,
                })
                .await
                .map(|response| response.branch.map(SharedString::from)),
        };
        default_branch
            .context(i18n::t!("Could not look up the default branch"))
            .log_err()
            .flatten()
    }

    /// Where the merge-request branch points before this build moves it, refusing to touch an
    /// existing one unless the user asked for it to be recreated.
    async fn merge_request_previous_sha_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        merge_request_branch: &str,
        recreate: bool,
    ) -> Result<Option<String>> {
        git::repository::validate_merge_request_branch_name(merge_request_branch)?;
        let default_branch = Self::local_default_branch_in_job(repo, id).await;
        git::repository::ensure_merge_request_branch_is_not_default(
            merge_request_branch,
            default_branch.as_deref(),
        )?;
        let previous_sha =
            Self::verify_ref_in_job(repo, id, &format!("refs/heads/{merge_request_branch}"))
                .await?;
        anyhow::ensure!(
            previous_sha.is_none() || recreate,
            i18n::t!(
                "Branch \"{merge_request_branch}\" already exists. To recreate it, check \"Recreate the existing {merge_request_branch}\"."
            )
        );
        Ok(previous_sha)
    }

    /// Replaces the consecutive range `oldest_sha..=newest_sha` on the current branch with
    /// `groups`, which is what the todo editor collects: commits reordered, folded together,
    /// reworded or left out. `expected_shas` is every commit the editor showed for the range, kept
    /// or dropped: git's own idea of the range has to be exactly that, so that a commit nobody
    /// saw is never dropped along with the ones the user left out.
    ///
    /// The whole rewrite happens on a scratch branch and the user's branch only moves at the
    /// final `rebase --onto` (see [`git::repository::rewrite_range_commands`]), so a step git
    /// refuses leaves the branch where it was. A conflict is git refusing rather than an error:
    /// the scratch branch then stays, and the git panel's banner is where the stopped
    /// `cherry-pick` or `rebase` is continued or aborted.
    pub fn rewrite_range(
        &mut self,
        oldest_sha: String,
        newest_sha: String,
        groups: Vec<git::repository::RewriteGroup>,
        expected_shas: Vec<String>,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let branch_name = self.current_branch_name();
        let scratch_branch = git::repository::rewrite_scratch_branch_name(&newest_sha);
        let receiver = self.send_job(
            "rewrite_range",
            Some(format!("git switch -c {scratch_branch}").into()),
            move |repo, _cx| async move {
                // A detached HEAD has no branch for the final `rebase --onto` to move, and the
                // scratch branch would take HEAD's place with nothing to give it back.
                let branch = branch_name.context(i18n::t!(
                    "A detached HEAD cannot be reordered. Switch to a branch first."
                ))?;
                let base = Self::parent_of_in_job(&repo, id, &oldest_sha)
                    .await?
                    .context(i18n::t!(
                        "A range with the first commit cannot be reordered. Choose again from the commit after it."
                    ))?;
                Self::ensure_commit_is_in_head_in_job(&repo, id, &newest_sha).await?;
                if oldest_sha != newest_sha {
                    Self::ensure_commit_is_in_head_in_job(&repo, id, &oldest_sha).await?;
                }
                Self::ensure_range_is_linear_in_job(&repo, id, format!("{base}..{newest_sha}"))
                    .await?;
                // The scratch branch starts at `base` and only `groups` are applied onto it, so a
                // commit of the range the list never showed would vanish from the branch as if
                // the user had dropped it, and a commit of `groups` from outside the range, or one
                // applied twice, would put changes into the branch that were never part of it.
                let range = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "log".into(),
                        "--format=%H".into(),
                        format!("{base}..{newest_sha}"),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    range.succeeded(),
                    i18n::t!(
                        "Could not get the commits in the range to rewrite: {error}",
                        error = range.failure_message()
                    )
                );
                let range_shas: HashSet<&str> = range
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect();
                anyhow::ensure!(
                    range_shas.contains(oldest_sha.as_str()),
                    i18n::t!(
                        "The oldest commit of the chosen range is not in the history of the newest one. Choose the range again."
                    )
                );
                let listed_shas: HashSet<&str> = expected_shas.iter().map(String::as_str).collect();
                anyhow::ensure!(
                    listed_shas.len() == expected_shas.len() && listed_shas == range_shas,
                    i18n::t!(
                        "The listed commits do not match the actual range (the git history). Commits may have been added to or removed from the range. Refresh the graph and choose again."
                    )
                );
                let mut applied: HashSet<&str> = HashSet::default();
                for sha in groups.iter().flat_map(|group| &group.shas) {
                    let short = sha.get(..7).unwrap_or(sha);
                    anyhow::ensure!(
                        listed_shas.contains(sha.as_str()),
                        i18n::t!(
                            "Commit {short} is not in the range to rewrite. Choose the range again."
                        )
                    );
                    anyhow::ensure!(
                        applied.insert(sha.as_str()),
                        i18n::t!("Commit {short} is included twice. Check the list.")
                    );
                }
                // The closing `rebase --onto` replays every commit after the range, and a plain
                // rebase flattens a merge commit it meets there without a word, so a merge anywhere
                // between the range and HEAD stops the rewrite as well.
                let merges_after = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "log".into(),
                        "--merges".into(),
                        "--format=%H".into(),
                        format!("{newest_sha}..HEAD"),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    merges_after.succeeded(),
                    i18n::t!(
                        "Could not examine the commits after the range to rewrite: {error}",
                        error = merges_after.failure_message()
                    )
                );
                anyhow::ensure!(
                    merges_after.stdout.trim().is_empty(),
                    i18n::t!(
                        "A merge commit is among the commits newer than the chosen range, so it cannot be rewritten (rewriting would break that merge). Choose only commits newer than the merge."
                    )
                );

                let leftover = Self::run_step_in_job(
                    &repo,
                    id,
                    vec![
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        format!("refs/heads/{scratch_branch}"),
                    ],
                )
                .await?;
                anyhow::ensure!(
                    !leftover.succeeded(),
                    i18n::t!(
                        "The scratch branch \"{scratch_branch}\" of the previous rewrite is still there. Check its contents and delete it, then try again."
                    )
                );
                Self::ensure_no_ignored_file_is_overwritten_in_job(
                    &repo,
                    id,
                    git::repository::RewrittenCommits::Range(&format!("{base}..HEAD")),
                )
                .await?;

                let mut commands = git::repository::rewrite_range_commands(
                    &base,
                    &groups,
                    &newest_sha,
                    &branch,
                    &scratch_branch,
                )?
                .into_iter();
                let create_scratch = commands
                    .next()
                    .context(i18n::t!("Could not build the commands for the rewrite."))?;
                let stash_before = Self::stash_top_in_job(&repo, id).await?;
                let created = Self::run_step_in_job(&repo, id, create_scratch.args).await?;
                if !created.succeeded() {
                    return Ok(created);
                }

                let outcome = Self::run_rewrite_commands_in_job(&repo, id, commands).await;
                let stopped = match &outcome {
                    Ok(output) => !output.succeeded(),
                    Err(_) => true,
                };
                if stopped {
                    for args in
                        git::repository::rewrite_cleanup_commands(&branch, &scratch_branch)?
                    {
                        match Self::run_step_in_job(&repo, id, args).await {
                            Ok(output) if output.succeeded() => {}
                            // A stopped `cherry-pick` or `rebase` keeps `switch` from leaving the
                            // scratch branch, and that is the state the panel's banner needs to
                            // offer continuing or aborting, so the rest of the cleanup is left
                            // undone on purpose.
                            Ok(_) => break,
                            Err(error) => {
                                log::warn!("could not clean up after the rewrite: {error:#}");
                                break;
                            }
                        }
                    }
                }
                if outcome.as_ref().is_ok_and(GitCommandOutput::succeeded)
                    && Self::stash_top_in_job(&repo, id).await? != stash_before
                {
                    return Err(ChangesLeftInStash.into());
                }
                outcome
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Runs a rewrite's commands in order, stopping at the first one git refuses, giving each the
    /// `GIT_AUTHOR_*` of the commit it has to keep.
    async fn run_rewrite_commands_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        commands: impl IntoIterator<Item = git::repository::RewriteCommand>,
    ) -> Result<GitCommandOutput> {
        let mut last = GitCommandOutput::default();
        for command in commands {
            let author = match &command.author_of {
                Some(sha) => Self::author_env_in_job(repo, id, sha).await?,
                None => HashMap::default(),
            };
            last = Self::run_command_in_job(repo, id, command.args, None, author).await?;
            if !last.succeeded() {
                break;
            }
        }
        Ok(last)
    }

    /// Leaves the scratch branch a stopped rewrite parked the user on: back to `branch`, then the
    /// scratch branch is deleted.
    ///
    /// This is what the git panel's banner runs after aborting the stopped `cherry-pick`, because
    /// aborting on its own only undoes the pick and leaves the user on a branch they never asked
    /// for. The half-rewritten commits go with it, which is why the banner says so before the user
    /// presses anything (they are still in the reflog).
    pub fn leave_rewrite_scratch_branch(
        &mut self,
        branch: String,
        scratch_branch: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let receiver = self.send_job(
            "leave_rewrite_scratch_branch",
            Some(format!("git switch {branch}").into()),
            move |repo, _cx| async move {
                let steps = git::repository::rewrite_cleanup_commands(&branch, &scratch_branch)?;
                Self::run_steps_in_job(&repo, id, steps).await
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Drops one commit out of the current branch's history: everything after it is replayed onto
    /// its parent. The commits after it get new SHAs, so an already-pushed branch needs a force
    /// push afterwards.
    pub fn drop_commit(
        &mut self,
        sha: String,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        let id = self.id;
        let branch_name = self.current_branch_name();
        let receiver = self.send_job(
            "drop_commit",
            Some(format!("git rebase --onto {sha}^").into()),
            move |repo, _cx| async move {
                let parent = Self::parent_of_in_job(&repo, id, &sha).await?.context(i18n::t!(
                    "The first commit cannot be dropped. The repository's first commit has no parent."
                ))?;
                Self::ensure_commit_is_in_head_in_job(&repo, id, &sha).await?;
                Self::ensure_range_is_linear_in_job(&repo, id, format!("{sha}..HEAD")).await?;
                Self::ensure_no_ignored_file_is_overwritten_in_job(
                    &repo,
                    id,
                    git::repository::RewrittenCommits::Range(&format!("{parent}..HEAD")),
                )
                .await?;
                let stash_before = Self::stash_top_in_job(&repo, id).await?;
                let dropped = Self::run_step_in_job(
                    &repo,
                    id,
                    git::repository::rebase_onto_args(&parent, &sha, branch_name.as_deref()),
                )
                .await?;
                if dropped.succeeded() && Self::stash_top_in_job(&repo, id).await? != stash_before {
                    return Err(ChangesLeftInStash.into());
                }
                Ok(dropped)
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Checks that changes can be folded into `sha` before the user is asked which ones: the
    /// refusals [`Self::fixup_commit`] makes again when it runs, plus what the confirmation shows.
    pub fn fixup_target(
        &mut self,
        sha: String,
    ) -> oneshot::Receiver<Result<git::repository::FixupTarget>> {
        let id = self.id;
        self.send_job("fixup_target", None, move |repo, _cx| async move {
            let target = Self::validate_fixup_target_in_job(&repo, id, &sha).await?;
            let subject = Self::run_step_in_job(
                &repo,
                id,
                vec![
                    "log".into(),
                    "-1".into(),
                    "--format=%s".into(),
                    target.sha.clone(),
                ],
            )
            .await?;
            anyhow::ensure!(
                subject.succeeded(),
                i18n::t!(
                    "Could not read the commit subject: {error}",
                    error = subject.failure_message()
                )
            );
            let pushed_probe =
                Self::run_step_in_job(&repo, id, git::repository::pushed_probe_args(&target.sha))
                    .await?;
            let branch = Self::run_step_in_job(
                &repo,
                id,
                vec![
                    "rev-parse".into(),
                    "--symbolic-full-name".into(),
                    "HEAD".into(),
                ],
            )
            .await?;
            let branch = branch.stdout.trim().to_string();
            anyhow::ensure!(
                branch.starts_with("refs/heads/"),
                i18n::t!("Not available on a detached HEAD. Switch to a branch first.")
            );
            Ok(git::repository::FixupTarget {
                sha: target.sha,
                subject: subject.stdout.trim_end_matches(['\r', '\n']).to_string(),
                pushed: git::repository::is_pushed_from_probe(&pushed_probe),
                has_upstream: git::repository::has_upstream_from_probe(&pushed_probe),
                branch,
            })
        })
    }

    /// Folds `changes` into `sha`, a commit on the current branch (fixup): `commit --fixup`, then
    /// `rebase -i --autosquash` from the commit's parent. Every commit after `sha` gets a new SHA.
    ///
    /// git's own fixup machinery rather than a chain of cherry-picks, because the `fixup!` commit
    /// holds the changes whatever the rebase does: a conflict, and aborting it from the banner,
    /// leave them in that commit instead of losing them.
    pub fn fixup_commit(
        &mut self,
        sha: String,
        changes: git::repository::FixupChanges,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<Result<git::repository::FixupOutcome>> {
        use git::repository::{FixupChanges, FixupOutcome};

        let id = self.id;
        let untracked = match &changes {
            FixupChanges::Paths(paths) => self.untracked_paths_among(paths),
            FixupChanges::Staged | FixupChanges::AllTracked => Vec::new(),
        };
        let receiver = self.send_job(
            "fixup_commit",
            Some(format!("git commit --fixup={sha}").into()),
            move |repo, _cx| async move {
                let target = Self::validate_fixup_target_in_job(&repo, id, &sha).await?;
                Self::ensure_fixup_has_changes_in_job(&repo, id, &changes, &untracked).await?;
                Self::ensure_no_ignored_file_is_overwritten_in_job(
                    &repo,
                    id,
                    git::repository::RewrittenCommits::Range(&target.range),
                )
                .await?;

                let steps =
                    git::repository::fixup_commit_commands(&target.sha, &changes, &untracked)?;
                let committed = Self::run_steps_in_job(&repo, id, steps).await?;
                if !committed.succeeded() {
                    if let Some(unstage) = git::repository::fixup_unstage_args(&untracked) {
                        match Self::run_step_in_job(&repo, id, unstage).await {
                            Ok(output) if output.succeeded() => {}
                            Ok(output) => log::warn!(
                                "could not unstage the files added for the fixup: {}",
                                output.failure_message()
                            ),
                            Err(error) => log::warn!(
                                "could not unstage the files added for the fixup: {error:#}"
                            ),
                        }
                    }
                    let head = Self::resolve_head_in_job(&repo, id).await?;
                    return Ok(FixupOutcome::NotCommitted {
                        output: committed,
                        head_moved: head != target.head,
                    });
                }
                let fixup_commit = Self::resolve_head_in_job(&repo, id).await?;
                let uncommitted_after_commit = Self::uncommitted_changes_in_job(&repo, id).await?;
                let stash_before = Self::stash_top_in_job(&repo, id).await?;

                let rebase_failure = match Self::run_step_in_job(
                    &repo,
                    id,
                    git::repository::fixup_rebase_args(target.parent.as_deref()),
                )
                .await
                {
                    Ok(output) if output.succeeded() => None,
                    Ok(output) => Some(output.failure_message()),
                    // Refused before git ran: an older git, which has no `--autostash`, asked to
                    // rebase with changes left in the working tree.
                    Err(error) => Some(format!("{error:#}")),
                };
                if let Some(message) = rebase_failure {
                    if Self::rebase_in_progress_in_job(&repo, id).await? {
                        return Ok(FixupOutcome::RebaseStopped {
                            message,
                            old_head: target.head,
                            fixup_commit,
                        });
                    }
                    // Nothing was replayed, so taking the `fixup!` commit back off the branch
                    // (`--soft`, which leaves the index and the files alone) is the state before
                    // the fixup with its changes staged, rather than a commit the user never asked
                    // to keep on its own. Only when git left the index and the files as they
                    // were, though: a rebase that dies while detaching HEAD has already checked
                    // out part of the older tree, and moving the branch under that would stage
                    // it as the user's own change.
                    let head = Self::resolve_head_in_job(&repo, id).await?;
                    let untouched = head == fixup_commit
                        && Self::uncommitted_changes_in_job(&repo, id).await?
                            == uncommitted_after_commit;
                    let mut rolled_back = false;
                    if untouched {
                        match Self::run_step_in_job(
                            &repo,
                            id,
                            vec!["reset".into(), "--soft".into(), target.head.clone()],
                        )
                        .await
                        {
                            Ok(output) if output.succeeded() => rolled_back = true,
                            Ok(output) => log::warn!(
                                "could not undo the fixup! commit: {}",
                                output.failure_message()
                            ),
                            Err(error) => {
                                log::warn!("could not undo the fixup! commit: {error:#}")
                            }
                        }
                    }
                    return Ok(FixupOutcome::RebaseRefused {
                        message,
                        rolled_back,
                    });
                }

                let new_head = Self::resolve_head_in_job(&repo, id).await?;
                if Self::stash_top_in_job(&repo, id).await? != stash_before {
                    return Ok(FixupOutcome::ChangesLeftInStash {
                        old_head: target.head,
                        new_head,
                    });
                }
                let subjects_after = Self::run_step_in_job(
                    &repo,
                    id,
                    vec!["log".into(), "--format=%s".into(), target.range.clone()],
                )
                .await?;
                anyhow::ensure!(
                    subjects_after.succeeded(),
                    i18n::t!(
                        "Could not examine the rewritten range: {error}",
                        error = subjects_after.failure_message()
                    )
                );
                if git::repository::fixup_left_unfolded(&subjects_after.stdout) {
                    return Ok(FixupOutcome::LeftUnfolded {
                        old_head: target.head,
                        new_head,
                    });
                }
                Ok(FixupOutcome::Folded {
                    old_head: target.head,
                    new_head,
                })
            },
        );
        self.schedule_scan_after_worktree_change(cx);
        receiver
    }

    /// Whether the current branch's upstream already contains `sha`, which is when rewriting it
    /// leaves the branch needing a force push. Ask before the rewrite: the rewritten commits are
    /// new, and no upstream has them.
    pub fn commit_is_pushed(&mut self, sha: String) -> oneshot::Receiver<Result<bool>> {
        let id = self.id;
        self.send_job("commit_is_pushed", None, move |repo, _cx| async move {
            Self::commit_is_pushed_in_job(&repo, id, &sha).await
        })
    }

    /// The untracked files among `paths`, or under them when a path is a directory.
    fn untracked_paths_among(&self, paths: &[RepoPath]) -> Vec<RepoPath> {
        self.cached_status()
            .filter(|entry| entry.status.is_untracked())
            .filter(|entry| paths.iter().any(|path| entry.repo_path.starts_with(path)))
            .map(|entry| entry.repo_path)
            .collect()
    }

    /// `git show <committish>:<path>`: the contents of one file as of a ref, with `path`
    /// relative to the repository root and spelled with `/`. A path that does not exist there
    /// comes back as a non-zero exit code rather than an error.
    pub fn file_at_ref(
        &mut self,
        committish: String,
        path: String,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_git_command(vec!["show".into(), format!("{committish}:{path}")], None)
    }

    /// `git diff --name-status <base>...<head>`: what `head` changed since it left `base`.
    pub fn changed_files_between(
        &mut self,
        base: String,
        head: String,
    ) -> oneshot::Receiver<Result<GitCommandOutput>> {
        self.run_git_command(
            vec![
                "diff".into(),
                "--name-status".into(),
                format!("{base}...{head}"),
            ],
            None,
        )
    }

    /// The branch a history rewrite has to move, or `None` on a detached HEAD, where
    /// `rebase --onto` moves HEAD itself.
    fn current_branch_name(&self) -> Option<String> {
        self.branch.as_ref().map(|branch| branch.name().to_string())
    }

    /// `git commit-tree`, returning the new commit's SHA, or the output of the failed command.
    async fn commit_tree_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        tree: String,
        parent: Option<String>,
        message: String,
        author: HashMap<String, String>,
    ) -> Result<std::result::Result<String, GitCommandOutput>> {
        let mut args = vec!["commit-tree".to_string(), tree];
        if let Some(parent) = parent {
            args.extend(["-p".to_string(), parent]);
        }
        args.extend(["-m".to_string(), message]);
        let output = Self::run_command_in_job(repo, id, args, None, author).await?;
        if !output.succeeded() {
            return Ok(Err(output));
        }
        let new_sha = output.stdout.trim().to_string();
        anyhow::ensure!(
            !new_sha.is_empty(),
            i18n::t!("git commit-tree did not return a new commit.")
        );
        Ok(Ok(new_sha))
    }

    /// `GIT_AUTHOR_*` of `sha`, so a commit rebuilt with `commit-tree` keeps its original author.
    /// The committer becomes whoever is doing the rewrite, which is what `git rebase` does too.
    async fn author_env_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        sha: &str,
    ) -> Result<HashMap<String, String>> {
        // `--date=raw` rather than `%aI`: strict ISO 8601 needs git 2.2, and git prints an
        // unrecognized placeholder verbatim, which would then travel into GIT_AUTHOR_DATE
        // (ADR-F47).
        let output = Self::run_step_in_job(
            repo,
            id,
            vec![
                "log".into(),
                "-1".into(),
                "--format=%an%x00%ae%x00%ad".into(),
                "--date=raw".into(),
                sha.to_string(),
            ],
        )
        .await?;
        anyhow::ensure!(
            output.succeeded(),
            i18n::t!(
                "Could not get the author of the original commit: {error}",
                error = output.failure_message()
            )
        );
        let mut fields = output.stdout.trim_end_matches(['\r', '\n']).split('\0');
        let name = fields.next().unwrap_or_default();
        let email = fields.next().unwrap_or_default();
        let date = fields.next().unwrap_or_default();
        anyhow::ensure!(
            !name.is_empty() && !email.is_empty() && !date.is_empty(),
            i18n::t!(
                "Could not get the author of the original commit: cannot interpret the output of git log."
            )
        );
        Ok(HashMap::from_iter([
            ("GIT_AUTHOR_NAME".to_string(), name.to_string()),
            ("GIT_AUTHOR_EMAIL".to_string(), email.to_string()),
            ("GIT_AUTHOR_DATE".to_string(), date.to_string()),
        ]))
    }

    /// The parent of `sha`, or `None` when it is a root commit.
    async fn parent_of_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        sha: &str,
    ) -> Result<Option<String>> {
        let output = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("{sha}^"),
            ],
        )
        .await?;
        Ok(output.succeeded().then(|| output.stdout.trim().to_string()))
    }

    /// Refuses a commit that HEAD cannot reach.
    ///
    /// Every rewrite here ends in `rebase --onto <new> <sha> <branch>`, and that replays
    /// `<sha>..<branch>`. For a commit on some other branch — which the graph happily shows when
    /// it is listing every branch — that range is the whole of the current branch, so the rewrite
    /// would quietly replay all of it onto an unrelated commit instead of touching the one commit
    /// the user pointed at.
    async fn ensure_commit_is_in_head_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        sha: &str,
    ) -> Result<()> {
        let output = Self::run_step_in_job(
            repo,
            id,
            vec!["log".into(), "--format=%H".into(), format!("HEAD..{sha}")],
        )
        .await?;
        anyhow::ensure!(
            output.succeeded(),
            i18n::t!(
                "Could not check whether the commit is on the current branch: {error}",
                error = output.failure_message()
            )
        );
        anyhow::ensure!(
            output.stdout.trim().is_empty(),
            i18n::t!(
                "A commit that is not in the history of the current branch cannot be rewritten. Switch to a branch that has it, then try again."
            )
        );
        Ok(())
    }

    /// Refuses a range containing a merge commit: replaying it with `commit-tree` +
    /// `rebase --onto` would silently flatten the merged-in history.
    async fn ensure_range_is_linear_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        range: String,
    ) -> Result<()> {
        let output = Self::run_step_in_job(
            repo,
            id,
            vec!["log".into(), "--merges".into(), "--format=%H".into(), range],
        )
        .await?;
        anyhow::ensure!(
            output.succeeded(),
            i18n::t!(
                "Could not examine the range to rewrite: {error}",
                error = output.failure_message()
            )
        );
        anyhow::ensure!(
            output.stdout.trim().is_empty(),
            i18n::t!(
                "A range with a merge commit cannot be rewritten. Choose a range without merges."
            )
        );
        Ok(())
    }

    /// Refuses everything a fixup of `sha` cannot do safely, before it writes anything, and hands
    /// back the commits it is going to replay.
    async fn validate_fixup_target_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        sha: &str,
    ) -> Result<FixupRange> {
        // A caller's SHA is spliced into revision arguments, so anything else (an option, a range)
        // is refused rather than handed to git.
        anyhow::ensure!(
            sha.len() >= 4 && sha.chars().all(|character| character.is_ascii_hexdigit()),
            i18n::t!("Cannot read this as a commit SHA: {sha}")
        );
        let resolved = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("{sha}^{{commit}}"),
            ],
        )
        .await?;
        anyhow::ensure!(
            resolved.succeeded(),
            i18n::t!("Commit {sha} was not found.")
        );
        let sha = resolved.stdout.trim().to_string();
        let head = Self::resolve_head_in_job(repo, id).await?;

        for kind in SequencerKind::all() {
            let stopped = Self::run_step_in_job(
                repo,
                id,
                vec![
                    "rev-parse".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    kind.in_progress_ref().into(),
                ],
            )
            .await?;
            anyhow::ensure!(
                !stopped.succeeded(),
                i18n::t!(
                    "The {kind} stopped partway. Continue or abort it in the Git panel, then try again.",
                    kind = kind.label()
                )
            );
        }
        // `commit` would conclude the merge instead of making a `fixup!` commit.
        let merging = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                "MERGE_HEAD".into(),
            ],
        )
        .await?;
        anyhow::ensure!(
            !merging.succeeded(),
            i18n::t!("A merge is in progress. Finish or abort the merge, then try again.")
        );
        let unmerged = Self::run_step_in_job(
            repo,
            id,
            vec![
                "diff".into(),
                "--name-only".into(),
                "--diff-filter=U".into(),
            ],
        )
        .await?;
        anyhow::ensure!(
            unmerged.stdout.trim().is_empty(),
            i18n::t!("Some files have conflicts. Resolve them, then try again.")
        );
        // Also what a rebase stopped without a conflict looks like on a git too old for
        // `REBASE_HEAD`.
        anyhow::ensure!(
            !Self::head_is_detached_in_job(repo, id).await?,
            i18n::t!("Not available on a detached HEAD. Switch to a branch first.")
        );

        Self::ensure_commit_is_in_head_in_job(repo, id, &sha).await?;
        let parent = Self::parent_of_in_job(repo, id, &sha).await?;
        let range = match &parent {
            Some(parent) => format!("{parent}..HEAD"),
            // The first commit of the repository: the rebase replays all of history.
            None => "HEAD".to_string(),
        };
        // The target itself is in the range: `rebase -i <parent>` would flatten a merge commit
        // being fixed up just as it flattens one after it.
        Self::ensure_range_is_linear_in_job(repo, id, range.clone()).await?;

        let subjects = Self::run_step_in_job(
            repo,
            id,
            vec!["log".into(), "--format=%s".into(), range.clone()],
        )
        .await?;
        anyhow::ensure!(
            subjects.succeeded(),
            i18n::t!(
                "Could not examine the range to rewrite: {error}",
                error = subjects.failure_message()
            )
        );
        if let Some(subject) = subjects
            .stdout
            .lines()
            .find(|subject| git::repository::is_autosquash_subject(subject))
        {
            anyhow::bail!(i18n::t!(
                "The range to rewrite has \"{subject}\". This operation would fold fixup! / squash! commits in along with it, so deal with them first, then try again."
            ));
        }

        Ok(FixupRange {
            sha,
            parent,
            head,
            range,
        })
    }

    /// Refuses a fixup that would record nothing, which git would otherwise report in English
    /// as a failed commit.
    async fn ensure_fixup_has_changes_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        changes: &git::repository::FixupChanges,
        untracked: &[RepoPath],
    ) -> Result<()> {
        use git::repository::FixupChanges;

        if matches!(changes, FixupChanges::Paths(_)) && !untracked.is_empty() {
            return Ok(());
        }
        let output =
            Self::run_step_in_job(repo, id, git::repository::fixup_changes_probe_args(changes))
                .await?;
        anyhow::ensure!(
            output.succeeded(),
            i18n::t!(
                "Could not examine the changes to fold in: {error}",
                error = output.failure_message()
            )
        );
        anyhow::ensure!(
            !output.stdout.trim().is_empty(),
            match changes {
                FixupChanges::Staged => i18n::t!("There are no staged changes."),
                FixupChanges::AllTracked => i18n::t!(
                    "There are no changes to put in the commit (untracked files go in only once staged)."
                ),
                FixupChanges::Paths(_) => i18n::t!("The chosen files have no changes."),
            }
        );
        Ok(())
    }

    /// The staged changes down to their contents, and the paths with unstaged ones: enough to
    /// tell whether a git command that failed left the index and the working tree alone.
    async fn uncommitted_changes_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
    ) -> Result<(String, String)> {
        let staged = Self::run_step_in_job(
            repo,
            id,
            vec![
                "diff".into(),
                "--cached".into(),
                "--raw".into(),
                "--no-abbrev".into(),
            ],
        )
        .await?;
        let unstaged =
            Self::run_step_in_job(repo, id, vec!["diff".into(), "--name-only".into()]).await?;
        anyhow::ensure!(
            staged.succeeded() && unstaged.succeeded(),
            i18n::t!(
                "Could not examine the state of the working tree: {error}",
                error = if staged.succeeded() {
                    unstaged.failure_message()
                } else {
                    staged.failure_message()
                }
            )
        );
        Ok((staged.stdout, unstaged.stdout))
    }

    async fn resolve_head_in_job(repo: &RepositoryState, id: RepositoryId) -> Result<String> {
        let head = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                "HEAD".into(),
            ],
        )
        .await?;
        anyhow::ensure!(
            head.succeeded(),
            i18n::t!(
                "Could not resolve HEAD: {error}",
                error = head.failure_message()
            )
        );
        Ok(head.stdout.trim().to_string())
    }

    async fn head_is_detached_in_job(repo: &RepositoryState, id: RepositoryId) -> Result<bool> {
        let head = Self::run_step_in_job(
            repo,
            id,
            vec!["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
        )
        .await?;
        // A repository without a first commit prints `HEAD` too, on its way to failing.
        Ok(head.succeeded() && head.stdout.trim() == "HEAD")
    }

    /// Whether a rebase that did not finish is waiting for the user, as opposed to one that never
    /// started. Every rebase detaches HEAD while it runs, which is the only sign an older git
    /// without `REBASE_HEAD` leaves.
    async fn rebase_in_progress_in_job(repo: &RepositoryState, id: RepositoryId) -> Result<bool> {
        let rebase_head = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                SequencerKind::Rebase.in_progress_ref().into(),
            ],
        )
        .await?;
        if rebase_head.succeeded() {
            return Ok(true);
        }
        Self::head_is_detached_in_job(repo, id).await
    }

    async fn commit_is_pushed_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        sha: &str,
    ) -> Result<bool> {
        let output =
            Self::run_step_in_job(repo, id, git::repository::pushed_probe_args(sha)).await?;
        Ok(git::repository::is_pushed_from_probe(&output))
    }

    /// The newest stash entry, if any. A rewrite whose `--autostash` could not put the user's
    /// changes back leaves a new one, and exits as if nothing went wrong.
    async fn stash_top_in_job(repo: &RepositoryState, id: RepositoryId) -> Result<Option<String>> {
        let output = Self::run_step_in_job(
            repo,
            id,
            vec![
                "rev-parse".into(),
                "--verify".into(),
                "--quiet".into(),
                "refs/stash".into(),
            ],
        )
        .await?;
        Ok(output.succeeded().then(|| output.stdout.trim().to_string()))
    }

    /// Refuses a history rewrite that would check out or replay a commit touching a path where the
    /// working tree has an ignored, untracked file: git overwrites such a file without asking and
    /// deletes it for good once a later commit stops tracking the path (see
    /// [`git::repository::touched_paths_probe_commands`]).
    async fn ensure_no_ignored_file_is_overwritten_in_job(
        repo: &RepositoryState,
        id: RepositoryId,
        commits: git::repository::RewrittenCommits<'_>,
    ) -> Result<()> {
        let mut touched = BTreeSet::new();
        for args in git::repository::touched_paths_probe_commands(commits) {
            let output = Self::run_step_in_job(repo, id, args).await?;
            anyhow::ensure!(
                output.succeeded(),
                i18n::t!(
                    "Could not examine the files the rewrite changes: {error}",
                    error = output.failure_message()
                )
            );
            touched.extend(
                git::repository::parse_nul_separated_paths(&output.stdout).map(str::to_string),
            );
        }
        let mut ignored = Vec::new();
        for args in
            git::repository::ignored_files_probe_commands(touched.iter().map(String::as_str))
        {
            let output = Self::run_step_in_job(repo, id, args)
                .await
                .map_err(explain_old_server_refusing_ls_files)?;
            anyhow::ensure!(
                output.succeeded(),
                i18n::t!(
                    "Could not examine the ignored local files: {error}",
                    error = output.failure_message()
                )
            );
            ignored.extend(
                git::repository::parse_nul_separated_paths(&output.stdout).map(str::to_string),
            );
        }
        if let Some(refusal) = git::repository::ignored_files_refusal(&ignored) {
            anyhow::bail!(refusal);
        }
        Ok(())
    }

    /// Refreshes git state after a command that may have rewritten the working
    /// tree, mirroring what [`Self::reset`] does.
    fn schedule_scan_after_worktree_change(&mut self, cx: &mut Context<Self>) {
        let scan_updates_tx =
            self.git_store()
                .and_then(|git_store| match &git_store.read(cx).state {
                    GitStoreState::Local { downstream, .. } => Some(
                        downstream
                            .as_ref()
                            .map(|downstream| downstream.updates_tx.clone()),
                    ),
                    _ => None,
                });
        if let Some(updates_tx) = scan_updates_tx {
            self.schedule_scan(updates_tx, cx);
        }
    }

    pub fn create_branch(
        &mut self,
        branch_name: String,
        base_branch: Option<String>,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let status_msg = if let Some(ref base) = base_branch {
            format!("git switch -c {branch_name} {base}").into()
        } else {
            format!("git switch -c {branch_name}").into()
        };
        self.send_job(
            "create_branch",
            Some(status_msg),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.create_branch(branch_name, base_branch).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitCreateBranch {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                branch_name,
                                base_branch,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn change_branch(&mut self, branch_name: String) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "change_branch",
            Some(format!("git switch {branch_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.change_branch(branch_name).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitChangeBranch {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                branch_name,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn delete_branch(
        &mut self,
        is_remote: bool,
        branch_name: String,
        force: bool,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        let flag = delete_branch_flag(is_remote, force);
        self.send_job(
            "delete_branch",
            Some(format!("git branch {flag} {branch_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(state) => {
                        state
                            .backend
                            .delete_branch(is_remote, branch_name, force)
                            .await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitDeleteBranch {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                is_remote,
                                branch_name,
                                force,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn rename_branch(
        &mut self,
        branch: String,
        new_name: String,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job(
            "rename_branch",
            Some(format!("git branch -m {branch} {new_name}").into()),
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.rename_branch(branch, new_name).await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        client
                            .request(proto::GitRenameBranch {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                                branch,
                                new_name,
                            })
                            .await?;

                        Ok(())
                    }
                }
            },
        )
    }

    pub fn check_for_pushed_commits(&mut self) -> oneshot::Receiver<Result<Vec<SharedString>>> {
        let id = self.id;
        self.send_job(
            "check_for_pushed_commits",
            None,
            move |repo, _cx| async move {
                match repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.check_for_pushed_commit().await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                        let response = client
                            .request(proto::CheckForPushedCommits {
                                project_id: project_id.0,
                                repository_id: id.to_proto(),
                            })
                            .await?;

                        let branches = response.pushed_to.into_iter().map(Into::into).collect();

                        Ok(branches)
                    }
                }
            },
        )
    }

    pub fn checkpoint(&mut self) -> oneshot::Receiver<Result<GitRepositoryCheckpoint>> {
        let id = self.id;
        self.send_job("checkpoint", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.checkpoint().await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitCreateCheckpoint {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                        })
                        .await?;

                    Ok(GitRepositoryCheckpoint {
                        commit_sha: Oid::from_bytes(&response.commit_sha)?,
                    })
                }
            }
        })
    }

    pub fn restore_checkpoint(
        &mut self,
        checkpoint: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<()>> {
        let id = self.id;
        self.send_job("restore_checkpoint", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.restore_checkpoint(checkpoint).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    client
                        .request(proto::GitRestoreCheckpoint {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            commit_sha: checkpoint.commit_sha.as_bytes().to_vec(),
                        })
                        .await?;
                    Ok(())
                }
            }
        })
    }

    pub(crate) fn apply_remote_update(
        &mut self,
        update: proto::UpdateRepository,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if let Some(repository_dir_abs_path) = &update.repository_dir_abs_path {
            self.snapshot.repository_dir_abs_path =
                Path::new(repository_dir_abs_path.as_str()).into();
        }
        if let Some(common_dir_abs_path) = &update.common_dir_abs_path {
            self.snapshot.common_dir_abs_path = Path::new(common_dir_abs_path.as_str()).into();
        }
        self.snapshot.opened_by_user = update.opened_by_user;

        let new_branch = update.branch_summary.as_ref().map(proto_to_branch);
        let new_head_commit = update
            .head_commit_details
            .as_ref()
            .map(proto_to_commit_details);
        if self.snapshot.branch != new_branch || self.snapshot.head_commit != new_head_commit {
            cx.emit(RepositoryEvent::HeadChanged)
        }
        self.snapshot.branch = new_branch;
        self.snapshot.head_commit = new_head_commit;

        if update.is_last_update && !update.branch_list_unchanged {
            let new_branch_list: Arc<[Branch]> =
                update.branch_list.iter().map(proto_to_branch).collect();
            let new_branch_list_error = update.branch_list_error.map(SharedString::from);
            if *self.snapshot.branch_list != *new_branch_list
                || self.snapshot.branch_list_error != new_branch_list_error
            {
                cx.emit(RepositoryEvent::BranchListChanged);
            }
            self.snapshot.branch_list = new_branch_list;
            self.snapshot.branch_list_error = new_branch_list_error;
        }
        if update.is_last_update {
            self.snapshot.loose_refs = update.loose_ref_count.map(|count| LooseRefCount {
                count: count as usize,
                complete: !update.loose_ref_count_is_lower_bound,
            });
            self.set_tags_fingerprint(update.tags_fingerprint, cx);
        }

        // We don't store any merge head state for downstream projects; the upstream
        // will track it and we will just get the updated conflicts
        let new_merge_heads = TreeMap::from_ordered_entries(
            update
                .current_merge_conflicts
                .into_iter()
                .filter_map(|path| Some((RepoPath::from_proto(&path).ok()?, vec![]))),
        );
        let conflicts_changed =
            self.snapshot.merge.merge_heads_by_conflicted_path != new_merge_heads;
        self.snapshot.merge.merge_heads_by_conflicted_path = new_merge_heads;
        self.snapshot.merge.message = update.merge_message.map(SharedString::from);
        let new_stash_entries = GitStash {
            entries: update
                .stash_entries
                .iter()
                .filter_map(|entry| proto_to_stash(entry).ok())
                .collect(),
        };
        if self.snapshot.stash_entries != new_stash_entries {
            cx.emit(RepositoryEvent::StashEntriesChanged)
        }
        self.snapshot.stash_entries = new_stash_entries;
        let new_linked_worktrees: Arc<[GitWorktree]> = update
            .linked_worktrees
            .iter()
            .map(proto_to_worktree)
            .collect();
        if *self.snapshot.linked_worktrees != *new_linked_worktrees {
            cx.emit(RepositoryEvent::GitWorktreeListChanged);
        }
        self.snapshot.linked_worktrees = new_linked_worktrees;
        self.snapshot.remote_upstream_url = update.remote_upstream_url;
        self.snapshot.remote_origin_url = update.remote_origin_url;

        let edits = update
            .removed_statuses
            .into_iter()
            .filter_map(|path| {
                Some(sum_tree::Edit::Remove(PathKey(
                    RelPath::from_unix_str(&path).log_err()?.into(),
                )))
            })
            .chain(
                update
                    .updated_statuses
                    .into_iter()
                    .filter_map(|updated_status| {
                        Some(sum_tree::Edit::Insert(updated_status.try_into().log_err()?))
                    }),
            )
            .collect::<Vec<_>>();
        // Intermediate chunks of a split update never carry the error.
        let mut status_error_changed = false;
        if update.is_last_update {
            let status_error = update.status_error.map(SharedString::from);
            status_error_changed = self.snapshot.status_error != status_error;
            self.snapshot.status_error = status_error;
        }
        if conflicts_changed || !edits.is_empty() || status_error_changed {
            cx.emit(RepositoryEvent::StatusesChanged);
        }
        self.snapshot.statuses_by_path.edit(edits, ());

        if update.is_last_update {
            self.snapshot.scan_id = update.scan_id;
        }
        self.clear_pending_ops(cx);
        Ok(())
    }

    pub fn compare_checkpoints(
        &mut self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<bool>> {
        let id = self.id;
        self.send_job("compare_checkpoints", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend.compare_checkpoints(left, right).await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitCompareCheckpoints {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            left_commit_sha: left.commit_sha.as_bytes().to_vec(),
                            right_commit_sha: right.commit_sha.as_bytes().to_vec(),
                        })
                        .await?;
                    Ok(response.equal)
                }
            }
        })
    }

    pub fn diff_checkpoints(
        &mut self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> oneshot::Receiver<Result<String>> {
        let id = self.id;
        self.send_job("diff_checkpoints", None, move |repo, _cx| async move {
            match repo {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    backend
                        .diff_checkpoints(base_checkpoint, target_checkpoint)
                        .await
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::GitDiffCheckpoints {
                            project_id: project_id.0,
                            repository_id: id.to_proto(),
                            base_commit_sha: base_checkpoint.commit_sha.as_bytes().to_vec(),
                            target_commit_sha: target_checkpoint.commit_sha.as_bytes().to_vec(),
                        })
                        .await?;
                    Ok(response.diff)
                }
            }
        })
    }

    fn clear_pending_ops(&mut self, cx: &mut Context<Self>) {
        let updated = SumTree::from_iter(
            self.pending_ops.iter().filter_map(|ops| {
                let inner_ops: Vec<PendingOp> =
                    ops.ops.iter().filter(|op| op.running()).cloned().collect();
                if inner_ops.is_empty() {
                    None
                } else {
                    Some(PendingOps {
                        repo_path: ops.repo_path.clone(),
                        ops: inner_ops,
                    })
                }
            }),
            (),
        );

        if updated != self.pending_ops {
            cx.emit(RepositoryEvent::PendingOpsChanged {
                pending_ops: self.pending_ops.clone(),
            })
        }

        self.pending_ops = updated;
    }

    fn schedule_scan(
        &mut self,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut Context<Self>,
    ) {
        self.schedule_partial_scan(SnapshotScan::FULL, updates_tx, cx)
    }

    fn schedule_partial_scan(
        &mut self,
        scan: SnapshotScan,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut Context<Self>,
    ) {
        if scan == SnapshotScan::NONE {
            return;
        }
        self.pending_snapshot_scan = self.pending_snapshot_scan.union(scan);
        let this = cx.weak_entity();
        let _ = self.send_keyed_job(
            "schedule_scan",
            Some(GitJobKey::ReloadGitState),
            None,
            move |state, mut cx| async move {
                let Some(this) = this.upgrade() else {
                    return Ok(());
                };
                // Whatever this job was asked for, plus anything requested by
                // jobs that the queue coalesced away in the meantime.
                let scan = this
                    .update(&mut cx, |this, _| {
                        mem::replace(&mut this.pending_snapshot_scan, SnapshotScan::NONE)
                    })
                    .union(scan);
                log::debug!("run scheduled git scan: {scan:?}");

                let RepositoryState::Local(LocalRepositoryState { backend, .. }) = state else {
                    bail!("not a local repository")
                };
                let snapshot = compute_snapshot(this.clone(), backend.clone(), scan, &mut cx).await;
                if scan.statuses {
                    // Optimistic staging state may only be dropped once real
                    // statuses have been read back.
                    this.update(&mut cx, |this, cx| {
                        this.clear_pending_ops(cx);
                    });
                }
                if let Some(updates_tx) = updates_tx {
                    updates_tx
                        .unbounded_send(DownstreamUpdate::UpdateRepository(snapshot))
                        .ok();
                }
                Ok(())
            },
        );
    }

    fn spawn_local_git_worker(
        state: Shared<Task<Result<LocalRepositoryState, String>>>,
        cx: &mut Context<Self>,
    ) -> (mpsc::UnboundedSender<GitJob>, Task<()>) {
        let (job_tx, mut job_rx) = mpsc::unbounded::<GitJob>();

        let worker_task = cx.spawn(async move |this, cx| {
            let state = match state.await {
                Ok(state) => state,
                Err(error) => {
                    log::error!("{error}");
                    // No job runs without a backend, including the status scans
                    // that would otherwise report why the repository is empty.
                    this.update(cx, |repository, cx| {
                        repository.set_status_error(Some(error.into()), cx);
                        if let Some(updates_tx) = repository.downstream_updates_tx(cx) {
                            updates_tx
                                .unbounded_send(DownstreamUpdate::UpdateRepository(
                                    repository.snapshot(),
                                ))
                                .ok();
                        }
                    })
                    .ok();
                    return;
                }
            };
            if let Some(git_hosting_provider_registry) =
                cx.update(|cx| GitHostingProviderRegistry::try_global(cx))
            {
                git_hosting_providers::register_additional_providers(
                    git_hosting_provider_registry,
                    state.backend.clone(),
                )
                .await;
            }
            let state = RepositoryState::Local(state);
            let mut jobs = VecDeque::new();
            loop {
                while let Ok(next_job) = job_rx.try_recv() {
                    jobs.push_back(next_job);
                }

                if let Some(job) = jobs.pop_front() {
                    if let Some(current_key) = &job.key
                        && jobs
                            .iter()
                            .any(|other_job| other_job.key.as_ref() == Some(current_key))
                    {
                        let skipped_job_id = job.id;
                        this.update(cx, |repo, _| {
                            repo.job_debug_queue.mark_complete(
                                skipped_job_id,
                                job_debug_queue::CompletedJobStatus::Skipped,
                            );
                        })
                        .ok();
                        continue;
                    }
                    // ADR-F93: the account picked now, over the project's environment.
                    (job.job)(crate::git_accounts::with_account_environment(&state), cx).await;
                } else if let Some(job) = job_rx.next().await {
                    jobs.push_back(job);
                } else {
                    break;
                }
            }
        });

        (job_tx, worker_task)
    }

    fn spawn_remote_git_worker(
        state: RemoteRepositoryState,
        cx: &mut Context<Self>,
    ) -> (mpsc::UnboundedSender<GitJob>, Task<()>) {
        let (job_tx, mut job_rx) = mpsc::unbounded::<GitJob>();

        let worker_task = cx.spawn(async move |this, cx| {
            let result: Result<()> = async {
                let state = RepositoryState::Remote(state);
                let mut jobs = VecDeque::new();
                loop {
                    while let Ok(next_job) = job_rx.try_recv() {
                        jobs.push_back(next_job);
                    }

                    if let Some(job) = jobs.pop_front() {
                        if let Some(current_key) = &job.key
                            && jobs
                                .iter()
                                .any(|other_job| other_job.key.as_ref() == Some(current_key))
                        {
                            let skipped_job_id = job.id;
                            this.update(cx, |repo, _| {
                                repo.job_debug_queue.mark_complete(
                                    skipped_job_id,
                                    job_debug_queue::CompletedJobStatus::Skipped,
                                );
                            })
                            .ok();
                            continue;
                        }
                        (job.job)(state.clone(), cx).await;
                    } else if let Some(job) = job_rx.next().await {
                        jobs.push_back(job);
                    } else {
                        break;
                    }
                }
                anyhow::Ok(())
            }
            .await;
            result.log_err();
        });

        (job_tx, worker_task)
    }

    fn load_staged_text(
        &mut self,
        buffer_id: BufferId,
        repo_path: RepoPath,
        cx: &App,
    ) -> Task<Result<Option<String>>> {
        let rx = self.send_job("load_staged_text", None, move |state, _| async move {
            match state {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => backend
                    .load_index_text(repo_path)
                    .await
                    .map(decode_git_text)
                    .transpose(),
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    let response = client
                        .request(proto::OpenUnstagedDiff {
                            project_id: project_id.to_proto(),
                            buffer_id: buffer_id.to_proto(),
                        })
                        .await?;
                    Ok(response.staged_text)
                }
            }
        });
        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    fn load_committed_text(
        &mut self,
        buffer_id: BufferId,
        repo_path: RepoPath,
        cx: &App,
    ) -> Task<Result<DiffBasesChange>> {
        let rx = self.send_job("load_committed_text", None, move |state, _| async move {
            match state {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    let revisions = vec![
                        format!("HEAD:{}", repo_path.as_unix_str()),
                        format!(":{}", repo_path.as_unix_str()),
                    ];
                    let mut loaded_revisions = backend.load_revisions(revisions).await?.into_iter();
                    let committed_text = loaded_revisions
                        .next()
                        .flatten()
                        .map(decode_git_text)
                        .transpose()?;
                    let staged_text = loaded_revisions
                        .next()
                        .flatten()
                        .map(decode_git_text)
                        .transpose()?;
                    let diff_bases_change = if committed_text == staged_text {
                        DiffBasesChange::SetBoth(committed_text)
                    } else {
                        DiffBasesChange::SetEach {
                            index: staged_text,
                            head: committed_text,
                        }
                    };
                    anyhow::Ok(diff_bases_change)
                }
                RepositoryState::Remote(RemoteRepositoryState { project_id, client }) => {
                    use proto::open_uncommitted_diff_response::Mode;

                    let response = client
                        .request(proto::OpenUncommittedDiff {
                            project_id: project_id.to_proto(),
                            buffer_id: buffer_id.to_proto(),
                        })
                        .await?;
                    let mode = Mode::try_from(response.mode).ok().context("Invalid mode")?;
                    let bases = match mode {
                        Mode::IndexMatchesHead => DiffBasesChange::SetBoth(response.committed_text),
                        Mode::IndexAndHead => DiffBasesChange::SetEach {
                            head: response.committed_text,
                            index: response.staged_text,
                        },
                    };
                    Ok(bases)
                }
            }
        });

        cx.spawn(|_: &mut AsyncApp| async move { rx.await? })
    }

    pub fn load_commit_template_text(
        &mut self,
    ) -> oneshot::Receiver<Result<Option<GitCommitTemplate>>> {
        let repository_id = self.snapshot.id;
        self.send_job(
            "load_commit_template_text",
            None,
            move |git_repo, _cx| async move {
                match git_repo {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        backend.load_commit_template().await
                    }
                    RepositoryState::Remote(RemoteRepositoryState { client, project_id }) => {
                        let response = client
                            .request(proto::LoadCommitTemplate {
                                project_id: project_id.to_proto(),
                                repository_id: repository_id.0,
                            })
                            .await?;
                        Ok(response
                            .template
                            .map(|template| GitCommitTemplate { template }))
                    }
                }
            },
        )
    }

    pub fn blame_buffer_at_revision(
        &mut self,
        path: RepoPath,
        revision: Oid,
        cx: &App,
    ) -> Task<Result<(String, git::blame::Blame)>> {
        let repository_id = self.snapshot.id;
        let rx = self.send_job("blame_buffer_at_revision", None, {
            let path = path.clone();
            move |state, _| async move {
                match state {
                    RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                        let content_task = backend
                            .load_revisions(vec![format!("{revision}:{}", path.as_unix_str())]);
                        let blame_task = backend.blame_at_revision(path.clone(), revision);
                        let (mut contents, blame) = futures::try_join!(content_task, blame_task)?;
                        let content = contents.pop().flatten().with_context(|| {
                            format!(
                                "cannot load {:?} at revision {revision}: the file is missing or binary",
                                path.as_ref()
                            )
                        })?;
                        let content = decode_git_text(content)?;
                        anyhow::Ok((content, blame))
                    }
                    RepositoryState::Remote(RemoteRepositoryState { client, project_id }) => {
                        let response = client
                            .request(proto::BlameBufferAtRevision {
                                project_id: project_id.to_proto(),
                                repository_id: repository_id.0,
                                path: path.as_unix_str().to_owned(),
                                revision: revision.to_string(),
                            })
                            .await?;
                        let blame = blame_from_proto(
                            response.entries,
                            response.messages,
                            response.tag_names,
                        );
                        Ok((response.content, blame))
                    }
                }
            }
        });
        cx.spawn(move |_: &mut AsyncApp| async move {
            let (content, blame) = rx.await??;
            anyhow::ensure!(
                !is_binary_content(content.as_bytes()),
                "cannot blame binary file {:?} at revision {revision}",
                path.as_ref()
            );
            Ok((content, blame))
        })
    }

    /// Bypasses the serial git job queue: blobs are content-addressed, so this read
    /// doesn't depend on the status snapshot, and a diff issues one per changed file.
    fn load_blob_content(&self, oid: Oid, cx: &App) -> Task<Result<String>> {
        let repository_id = self.snapshot.id;
        let repository_state = self.repository_state.clone();
        let blob_read_limiter = self.blob_read_limiter.clone();
        cx.background_spawn(async move {
            let _permit = blob_read_limiter.acquire_arc().await;
            match repository_state.await.map_err(|err| anyhow::anyhow!(err))? {
                RepositoryState::Local(LocalRepositoryState { backend, .. }) => {
                    decode_git_text(backend.load_blob_content(oid).await?)
                }
                RepositoryState::Remote(RemoteRepositoryState { client, project_id }) => {
                    let response = client
                        .request(proto::GetBlobContent {
                            project_id: project_id.to_proto(),
                            repository_id: repository_id.0,
                            oid: oid.to_string(),
                        })
                        .await?;
                    Ok(response.content)
                }
            }
        })
    }

    fn set_status_error(&mut self, status_error: Option<SharedString>, cx: &mut Context<Self>) {
        if self.snapshot.status_error != status_error {
            self.snapshot.status_error = status_error;
            cx.emit(RepositoryEvent::StatusesChanged);
        }
    }

    fn downstream_updates_tx(&self, cx: &App) -> Option<mpsc::UnboundedSender<DownstreamUpdate>> {
        match &self.git_store.upgrade()?.read(cx).state {
            GitStoreState::Local { downstream, .. } => downstream
                .as_ref()
                .map(|downstream| downstream.updates_tx.clone()),
            GitStoreState::Remote { .. } => None,
        }
    }

    fn paths_changed(
        &mut self,
        paths: Vec<RepoPath>,
        updates_tx: Option<mpsc::UnboundedSender<DownstreamUpdate>>,
        cx: &mut Context<Self>,
    ) {
        if !paths.is_empty() {
            self.paths_needing_status_update.push(paths);
        }

        let this = cx.weak_entity();
        let _ = self.send_keyed_job(
            "paths_changed",
            Some(GitJobKey::RefreshStatuses),
            None,
            |state, mut cx| async move {
                let (prev_snapshot, changed_paths, untracked_files) =
                    this.update(&mut cx, |this, cx| {
                        (
                            this.snapshot.clone(),
                            mem::take(&mut this.paths_needing_status_update),
                            ProjectSettings::get_global(cx).git.status_untracked_files,
                        )
                    })?;
                let RepositoryState::Local(LocalRepositoryState { backend, .. }) = state else {
                    bail!("not a local repository")
                };

                if changed_paths.is_empty() {
                    return Ok(());
                }

                let has_head = prev_snapshot.head_commit.is_some();

                let changed_path_statuses = cx
                    .background_spawn(async move {
                        let changed_paths = GitStore::coalesce_repo_paths(
                            changed_paths
                                .into_iter()
                                .flatten()
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect(),
                        );
                        let changed_paths_vec = changed_paths.iter().cloned().collect::<Vec<_>>();

                        let status_task = backend.status(&changed_paths_vec, untracked_files);
                        let diff_stat_future = |diff| {
                            if has_head {
                                backend.diff_stat(diff, &changed_paths_vec)
                            } else {
                                future::ready(Ok(status::GitDiffStat::default())).boxed()
                            }
                        };

                        let (statuses, diff_stats, staged_diff_stats, unstaged_diff_stats) =
                            futures::future::try_join4(
                                status_task,
                                diff_stat_future(DiffStatType::HeadToWorktree),
                                diff_stat_future(DiffStatType::HeadToIndex),
                                diff_stat_future(DiffStatType::IndexToWorktree),
                            )
                            .await?;

                        let diff_stats: HashMap<RepoPath, DiffStat> =
                            HashMap::from_iter(diff_stats.entries.into_iter().cloned());
                        let staged_diff_stats: HashMap<RepoPath, DiffStat> =
                            HashMap::from_iter(staged_diff_stats.entries.into_iter().cloned());
                        let unstaged_diff_stats: HashMap<RepoPath, DiffStat> =
                            HashMap::from_iter(unstaged_diff_stats.entries.into_iter().cloned());

                        let mut changed_path_statuses = Vec::new();
                        let prev_statuses = prev_snapshot.statuses_by_path.clone();
                        let current_status_paths = statuses
                            .entries
                            .iter()
                            .map(|(repo_path, _)| repo_path.clone())
                            .collect::<BTreeSet<_>>();

                        for path in &changed_paths {
                            let mut cursor = prev_statuses.cursor::<PathProgress>(());
                            cursor.seek_forward(&PathTarget::Path(path), Bias::Left);
                            while let Some(entry) = cursor.item() {
                                if !entry.repo_path.starts_with(path) {
                                    break;
                                }

                                if !current_status_paths.contains(&entry.repo_path) {
                                    changed_path_statuses.push(Edit::Remove(PathKey(
                                        entry.repo_path.as_ref().clone(),
                                    )));
                                }
                                cursor.next();
                            }
                        }

                        let mut cursor = prev_statuses.cursor::<PathProgress>(());

                        for (repo_path, status) in &*statuses.entries {
                            let current_diff_stat = diff_stats.get(repo_path).copied();
                            let current_staged_diff_stat =
                                staged_diff_stats.get(repo_path).copied();
                            let current_unstaged_diff_stat =
                                unstaged_diff_stats.get(repo_path).copied();

                            if cursor.seek_forward(&PathTarget::Path(repo_path), Bias::Left)
                                && cursor.item().is_some_and(|entry| {
                                    entry.status == *status
                                        && entry.diff_stat == current_diff_stat
                                        && entry.staged_diff_stat == current_staged_diff_stat
                                        && entry.unstaged_diff_stat == current_unstaged_diff_stat
                                })
                            {
                                continue;
                            }

                            changed_path_statuses.push(Edit::Insert(StatusEntry {
                                repo_path: repo_path.clone(),
                                status: *status,
                                diff_stat: current_diff_stat,
                                staged_diff_stat: current_staged_diff_stat,
                                unstaged_diff_stat: current_unstaged_diff_stat,
                            }));
                        }
                        anyhow::Ok(changed_path_statuses)
                    })
                    .await;
                let changed_path_statuses = match changed_path_statuses {
                    Ok(changed_path_statuses) => changed_path_statuses,
                    Err(error) => {
                        log::error!("failed to refresh git statuses of changed paths: {error:#}");
                        // The paths taken for this refresh are dropped, so a later
                        // successful refresh of other paths cannot clear this; only
                        // the next full status scan can.
                        this.update(&mut cx, |this, cx| {
                            this.set_status_error(Some(format!("{error:#}").into()), cx);
                            if let Some(updates_tx) = updates_tx {
                                updates_tx
                                    .unbounded_send(DownstreamUpdate::UpdateRepository(
                                        this.snapshot.clone(),
                                    ))
                                    .ok();
                            }
                        })?;
                        return Err(error);
                    }
                };

                this.update(&mut cx, |this, cx| {
                    if !changed_path_statuses.is_empty() {
                        cx.emit(RepositoryEvent::StatusesChanged);
                        this.snapshot
                            .statuses_by_path
                            .edit(changed_path_statuses, ());
                        this.snapshot.scan_id += 1;
                    }

                    if let Some(updates_tx) = updates_tx {
                        updates_tx
                            .unbounded_send(DownstreamUpdate::UpdateRepository(
                                this.snapshot.clone(),
                            ))
                            .ok();
                    }
                })
            },
        );
    }

    /// currently running git command and when it started
    pub fn current_job(&self) -> Option<JobInfo> {
        self.active_jobs.values().next().cloned()
    }

    pub fn job_debug_queue(&self) -> &job_debug_queue::GitJobDebugQueue {
        &self.job_debug_queue
    }

    pub fn barrier(&mut self) -> oneshot::Receiver<()> {
        self.send_job("barrier", None, |_, _| async {})
    }

    fn spawn_job_with_tracking<AsyncFn>(
        &mut self,
        paths: Vec<RepoPath>,
        git_status: pending_op::GitStatus,
        cx: &mut Context<Self>,
        f: AsyncFn,
    ) -> Task<Result<()>>
    where
        AsyncFn: AsyncFnOnce(WeakEntity<Repository>, &mut AsyncApp) -> Result<()> + 'static,
    {
        let ids = self.new_pending_ops_for_paths(paths, git_status);

        cx.spawn(async move |this, cx| {
            let (job_status, result) = match f(this.clone(), cx).await {
                Ok(()) => (pending_op::JobStatus::Finished, Ok(())),
                Err(err) if err.is::<Canceled>() => (pending_op::JobStatus::Skipped, Ok(())),
                Err(err) => (pending_op::JobStatus::Error, Err(err)),
            };

            this.update(cx, |this, _| {
                let mut edits = Vec::with_capacity(ids.len());
                for (id, entry) in ids {
                    if let Some(mut ops) = this
                        .pending_ops
                        .get(&PathKey(entry.as_ref().clone()), ())
                        .cloned()
                    {
                        if let Some(op) = ops.op_by_id_mut(id) {
                            op.job_status = job_status;
                        }
                        edits.push(sum_tree::Edit::Insert(ops));
                    }
                }
                this.pending_ops.edit(edits, ());
            })?;

            result
        })
    }

    fn new_pending_ops_for_paths(
        &mut self,
        paths: Vec<RepoPath>,
        git_status: pending_op::GitStatus,
    ) -> Vec<(PendingOpId, RepoPath)> {
        let mut edits = Vec::with_capacity(paths.len());
        let mut ids = Vec::with_capacity(paths.len());
        for path in paths {
            let mut ops = self
                .pending_ops
                .get(&PathKey(path.as_ref().clone()), ())
                .cloned()
                .unwrap_or_else(|| PendingOps::new(&path));
            let id = ops.max_id() + 1;
            ops.ops.push(PendingOp {
                id,
                git_status,
                job_status: pending_op::JobStatus::Running,
            });
            edits.push(sum_tree::Edit::Insert(ops));
            ids.push((id, path));
        }
        self.pending_ops.edit(edits, ());
        ids
    }

    pub fn access(&mut self, _cx: &App) -> oneshot::Receiver<GitAccess> {
        self.send_job("access", None, move |git_repo, _cx| async move {
            match git_repo {
                // TODO: Correctly handle remote repositories, where the user
                // that's running the Zed remote may not own the `.git/`
                // directory. For now we just return `GitAccess::Yes` so that
                // remoting continues working as expected.
                RepositoryState::Remote(..) => GitAccess::Yes,
                RepositoryState::Local(state) => match state.backend.check_access().await {
                    Ok(_) => GitAccess::Yes,
                    Err(_) => GitAccess::No,
                },
            }
        })
    }

    pub fn default_remote_url(&self) -> Option<String> {
        self.remote_upstream_url
            .clone()
            .or(self.remote_origin_url.clone())
    }
}

fn format_job_key(key: &GitJobKey) -> SharedString {
    match key {
        GitJobKey::WriteIndex(paths) => {
            let paths_str: Vec<_> = paths
                .iter()
                .map(|p| {
                    let rel: &RelPath = p;
                    rel.display(PathStyle::local())
                })
                .collect();
            format!("WriteIndex({})", paths_str.join(", ")).into()
        }
        GitJobKey::ReloadBufferDiffBases => "ReloadBufferDiffBases".into(),
        GitJobKey::RefreshStatuses => "RefreshStatuses".into(),
        GitJobKey::ReloadGitState => "ReloadGitState".into(),
        GitJobKey::SequencerState => "SequencerState".into(),
        GitJobKey::LatestTags(requester) => format!("LatestTags({requester})").into(),
        GitJobKey::LatestDescribe(requester) => format!("LatestDescribe({requester})").into(),
    }
}

/// If `path` is a git linked worktree checkout, resolves it to the main
/// repository's identity path. For regular linked worktrees this is the main
/// repository's working directory; for linked worktrees backed by a bare repo
/// such as `.bare`, this is the parent project directory users think of as the
/// repository root. Returns `None` if `path` is a normal repository, not a git
/// repo, or if resolution fails.
///
/// Resolution works by:
/// 1. Reading the `.git` file to get the `gitdir:` pointer
/// 2. Following that to the worktree-specific git directory
/// 3. Reading the `commondir` file to find the shared `.git` directory
/// 4. Deriving the main repo's identity path from the common dir
pub async fn resolve_git_worktree_to_main_repo(fs: &dyn Fs, path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    let metadata = fs.metadata(&dot_git).await.ok()??;
    if metadata.is_dir {
        return None; // Normal repo, not a linked worktree
    }
    // It's a .git file — parse the gitdir: pointer
    let content = fs.load(&dot_git).await.ok()?;
    let gitdir_rel = content.strip_prefix("gitdir:")?.trim();
    let gitdir_abs = fs.canonicalize(&path.join(gitdir_rel)).await.ok()?;
    // Submodules also use a `.git` file, but they are independent projects whose
    // identity is their own working directory (`path`), not the superproject's
    // `.git/modules/<name>` git dir. Leave them unresolved.
    if is_submodule_git_dir(&gitdir_abs) {
        return None;
    }
    // Read commondir to find the main .git directory
    let commondir_content = fs.load(&gitdir_abs.join("commondir")).await.ok()?;
    let common_dir = fs
        .canonicalize(&gitdir_abs.join(commondir_content.trim()))
        .await
        .ok()?;
    Some(repo_identity_path(&common_dir, PathStyle::local()).to_path_buf())
}

/// Validates that the resolved worktree directory is acceptable:
/// - The setting must not be an absolute path.
/// - The resolved path must be either a subdirectory of the working
///   directory or a subdirectory of its parent (i.e., a sibling).
///
/// Returns `Ok(resolved_path)` or an error with a user-facing message.
pub fn worktrees_directory_for_repo(
    repository_anchor_path: &Path,
    worktree_directory_setting: &str,
    path_style: PathStyle,
) -> Result<PathBuf> {
    // Check the original setting before trimming, since a path like "///"
    // is absolute but becomes "" after stripping trailing separators.
    // Also check for leading `/` or `\` explicitly, because on Windows
    // `Path::is_absolute()` requires a drive letter — so `/tmp/worktrees`
    // would slip through even though it's clearly not a relative path.
    if path_style.is_absolute(worktree_directory_setting)
        || worktree_directory_setting.starts_with('\\')
    {
        anyhow::bail!(
            "git.worktree_directory must be a relative path, got: {worktree_directory_setting:?}"
        );
    }

    if worktree_directory_setting.is_empty() {
        anyhow::bail!("git.worktree_directory must not be empty");
    }

    let trimmed = worktree_directory_setting.trim_end_matches(['/', '\\']);
    if trimmed == ".." {
        anyhow::bail!("git.worktree_directory must not be \"..\" (use \"../some-name\" instead)");
    }

    let joined = path_style.join_path(repository_anchor_path, trimmed)?;
    let resolved = if path_style.is_posix() {
        joined
    } else {
        path::normalize_path(&joined)
    };
    let resolved = if resolved.starts_with(repository_anchor_path) {
        resolved
    } else if let Some(repo_dir_name) = repository_anchor_path
        .file_name()
        .and_then(|name| name.to_str())
    {
        path_style.join_path(&resolved, repo_dir_name)?
    } else {
        resolved
    };

    let parent = repository_anchor_path
        .parent()
        .unwrap_or(repository_anchor_path);

    if !resolved.starts_with(parent) {
        anyhow::bail!(
            "git.worktree_directory resolved to {resolved:?}, which is outside \
             the project root and its parent directory. It must resolve to a \
             subdirectory of {repository_anchor_path:?} or a sibling of it."
        );
    }

    Ok(resolved)
}

async fn remove_empty_managed_worktree_ancestors(fs: &dyn Fs, child_path: &Path, base_path: &Path) {
    let mut current = child_path;
    while let Some(parent) = current.parent() {
        if parent == base_path {
            break;
        }
        if !parent.starts_with(base_path) {
            break;
        }

        let result = fs
            .remove_dir(
                parent,
                RemoveOptions {
                    recursive: false,
                    ignore_if_not_exists: true,
                },
            )
            .await;

        match result {
            Ok(()) => {
                log::info!(
                    "Removed empty managed worktree directory: {}",
                    parent.display()
                );
            }
            Err(error) => {
                log::debug!(
                    "Stopped removing managed worktree parent directories at {}: {error}",
                    parent.display()
                );
                break;
            }
        }

        current = parent;
    }
}

/// Returns the repository's identity path given its common Git directory.
///
/// This is the canonical, on-disk path used for project grouping and as the
/// basis for display names. The goal is to return the directory the user
/// thinks of as "the project":
///
/// - If `common_dir`'s last component starts with `.` (e.g. `.git` for a
///   normal checkout, or `.bare` for a bare clone), the parent directory is
///   returned. Both of these are internal Git directories; the parent is the
///   meaningful project root.
/// - Otherwise (e.g. `zed.git` for a bare clone), `common_dir` itself is
///   returned — it is already a meaningful on-disk path.
pub fn repo_identity_path(common_dir: &Path, path_style: PathStyle) -> &Path {
    let is_dot_entry = path_style
        .file_name(common_dir)
        .is_some_and(|n| n.starts_with('.'));
    if is_dot_entry {
        path_style.parent(common_dir).unwrap_or(common_dir)
    } else {
        common_dir
    }
}

/// Returns the repository identity only when `std::path` can interpret the path correctly.
///
/// Callers whose downstream path operations are not yet `PathStyle`-aware use this to preserve
/// their existing behavior for foreign path styles.
pub fn repo_identity_path_if_local(common_dir: &Path, path_style: PathStyle) -> Option<&Path> {
    (path_style == PathStyle::local()).then(|| repo_identity_path(common_dir, path_style))
}

/// Returns true if `git_dir` is a Git submodule's git directory.
///
/// Submodules store their git directory inside the superproject at
/// `<superproject>/.git/modules/<name>`. Unlike a linked worktree, a submodule
/// is an independent project whose identity is its own working directory, so its
/// path must not be resolved to the superproject's `.git/modules/...` directory
/// (from which the working directory cannot be derived).
pub fn is_submodule_git_dir(git_dir: &Path) -> bool {
    let mut previous_was_git_dir = false;
    for component in git_dir.components() {
        if let std::path::Component::Normal(name) = component {
            if previous_was_git_dir && name == std::ffi::OsStr::new("modules") {
                return true;
            }
            previous_was_git_dir = name.to_string_lossy().ends_with(".git");
        } else {
            previous_was_git_dir = false;
        }
    }
    false
}

/// Returns a short name for a linked worktree suitable for UI display
///
/// Uses the main worktree path to come up with a short name that disambiguates
/// the linked worktree from the main worktree.
pub fn linked_worktree_short_name(
    main_worktree_path: &Path,
    linked_worktree_path: &Path,
) -> Option<SharedString> {
    if main_worktree_path == linked_worktree_path {
        return None;
    }

    let project_name = main_worktree_path.file_name()?.to_str()?;
    let directory_name = linked_worktree_path.file_name()?.to_str()?;
    let name = if directory_name != project_name {
        directory_name.to_string()
    } else {
        linked_worktree_path
            .parent()?
            .file_name()?
            .to_str()?
            .to_string()
    };
    Some(name.into())
}

fn get_permalink_in_rust_registry_src(
    provider_registry: Arc<GitHostingProviderRegistry>,
    path: PathBuf,
    selection: Range<u32>,
) -> Result<url::Url> {
    #[derive(Deserialize)]
    struct CargoVcsGit {
        sha1: String,
    }

    #[derive(Deserialize)]
    struct CargoVcsInfo {
        git: CargoVcsGit,
        path_in_vcs: String,
    }

    #[derive(Deserialize)]
    struct CargoPackage {
        repository: String,
    }

    #[derive(Deserialize)]
    struct CargoToml {
        package: CargoPackage,
    }

    let Some((dir, cargo_vcs_info_json)) = path.ancestors().skip(1).find_map(|dir| {
        let json = std::fs::read_to_string(dir.join(".cargo_vcs_info.json")).ok()?;
        Some((dir, json))
    }) else {
        bail!("No .cargo_vcs_info.json found in parent directories")
    };
    let cargo_vcs_info = serde_json::from_str::<CargoVcsInfo>(&cargo_vcs_info_json)?;
    let cargo_toml = std::fs::read_to_string(dir.join("Cargo.toml"))?;
    let manifest = toml::from_str::<CargoToml>(&cargo_toml)?;
    let (provider, remote) = parse_git_remote_url(provider_registry, &manifest.package.repository)
        .context("parsing package.repository field of manifest")?;
    let path = PathBuf::from(cargo_vcs_info.path_in_vcs).join(path.strip_prefix(dir).unwrap());
    let permalink = provider.build_permalink(
        remote,
        BuildPermalinkParams::new(
            &cargo_vcs_info.git.sha1,
            &RepoPath::from_rel_path(
                &RelPath::new(&path, PathStyle::local()).context("invalid path")?,
            ),
            Some(selection),
        ),
    );
    Ok(permalink)
}

fn serialize_blame_buffer_response(blame: Option<git::blame::Blame>) -> proto::BlameBufferResponse {
    let Some(blame) = blame else {
        return proto::BlameBufferResponse {
            blame_response: None,
        };
    };

    let entries = blame
        .entries
        .into_iter()
        .map(serialize_blame_entry)
        .collect::<Vec<_>>();
    let messages = serialize_commit_messages(blame.messages);
    let tag_names = serialize_commit_tag_names(blame.tag_names);

    proto::BlameBufferResponse {
        blame_response: Some(proto::blame_buffer_response::BlameResponse {
            entries,
            messages,
            tag_names,
        }),
    }
}

fn serialize_blame_entry(entry: git::blame::BlameEntry) -> proto::BlameEntry {
    proto::BlameEntry {
        sha: entry.sha.as_bytes().into(),
        start_line: entry.range.start,
        end_line: entry.range.end,
        original_line_number: entry.original_line_number,
        author: entry.author,
        author_mail: entry.author_mail,
        author_time: entry.author_time,
        author_tz: entry.author_tz,
        committer: entry.committer_name,
        committer_mail: entry.committer_email,
        committer_time: entry.committer_time,
        committer_tz: entry.committer_tz,
        summary: entry.summary,
        previous: entry.previous,
        filename: entry.filename,
        boundary: entry.boundary,
    }
}

fn deserialize_blame_entry(entry: proto::BlameEntry) -> Option<git::blame::BlameEntry> {
    Some(git::blame::BlameEntry {
        sha: git::Oid::from_bytes(&entry.sha).ok()?,
        range: entry.start_line..entry.end_line,
        original_line_number: entry.original_line_number,
        committer_name: entry.committer,
        committer_time: entry.committer_time,
        committer_tz: entry.committer_tz,
        committer_email: entry.committer_mail,
        author: entry.author,
        author_mail: entry.author_mail,
        author_time: entry.author_time,
        author_tz: entry.author_tz,
        summary: entry.summary,
        previous: entry.previous,
        filename: entry.filename,
        boundary: entry.boundary,
    })
}

fn serialize_commit_messages(messages: HashMap<git::Oid, String>) -> Vec<proto::CommitMessage> {
    messages
        .into_iter()
        .map(|(oid, message)| proto::CommitMessage {
            oid: oid.as_bytes().into(),
            message,
        })
        .collect()
}

fn deserialize_commit_message(message: proto::CommitMessage) -> Option<(git::Oid, String)> {
    Some((git::Oid::from_bytes(&message.oid).ok()?, message.message))
}

fn serialize_commit_tag_names(
    tag_names: HashMap<git::Oid, Vec<String>>,
) -> Vec<proto::CommitTagNames> {
    tag_names
        .into_iter()
        .map(|(oid, tag_names)| proto::CommitTagNames {
            oid: oid.as_bytes().into(),
            tag_names,
        })
        .collect()
}

fn deserialize_commit_tag_names(
    tag_names: proto::CommitTagNames,
) -> Option<(git::Oid, Vec<String>)> {
    Some((
        git::Oid::from_bytes(&tag_names.oid).ok()?,
        tag_names.tag_names,
    ))
}

fn blame_from_proto(
    entries: Vec<proto::BlameEntry>,
    messages: Vec<proto::CommitMessage>,
    tag_names: Vec<proto::CommitTagNames>,
) -> git::blame::Blame {
    git::blame::Blame {
        entries: entries
            .into_iter()
            .filter_map(deserialize_blame_entry)
            .collect(),
        messages: messages
            .into_iter()
            .filter_map(deserialize_commit_message)
            .collect(),
        tag_names: tag_names
            .into_iter()
            .filter_map(deserialize_commit_tag_names)
            .collect(),
    }
}

fn deserialize_blame_buffer_response(
    response: proto::BlameBufferResponse,
) -> Option<git::blame::Blame> {
    let response = response.blame_response?;
    Some(blame_from_proto(
        response.entries,
        response.messages,
        response.tag_names,
    ))
}

/// What a `Push` request is really asking for.
///
/// `FORCE_WITHOUT_LEASE` is no longer sent: prost hands an unknown enum value back as the default
/// one, so a server that predates the value would read it as `SET_UPSTREAM` and push something
/// else entirely. A client that wants `--force` sends `FORCE` together with `force_without_lease`
/// instead, which such a server ignores, leaving it on `--force-with-lease`. The old value is
/// still honoured here for clients that predate the flag.
fn push_options_from_proto(
    options: Option<proto::push::PushOptions>,
    force_without_lease: bool,
) -> Option<PushOptions> {
    Some(match (options?, force_without_lease) {
        // The flag on its own is not enough: dropping the lease is only ever done because the
        // request said `FORCE` as well.
        (proto::push::PushOptions::SetUpstream, _) => PushOptions::SetUpstream,
        (proto::push::PushOptions::Force, false) => PushOptions::Force,
        (proto::push::PushOptions::Force, true)
        | (proto::push::PushOptions::ForceWithoutLease, _) => PushOptions::ForceWithoutLease,
    })
}

/// What the client asks a peer's server for when it wants a force push, paired so that the enum
/// and the flag can never disagree. See [`push_options_from_proto`].
fn push_options_to_proto(options: Option<PushOptions>) -> (Option<i32>, Option<bool>) {
    match options {
        None => (None, None),
        Some(PushOptions::SetUpstream) => {
            (Some(proto::push::PushOptions::SetUpstream as i32), None)
        }
        Some(PushOptions::Force) => (Some(proto::push::PushOptions::Force as i32), None),
        Some(PushOptions::ForceWithoutLease) => {
            (Some(proto::push::PushOptions::Force as i32), Some(true))
        }
    }
}

/// Whether a push that asked to drop the lease was answered by a server that never saw the ask.
///
/// The answer only carries the mark from a server new enough to have read the request, so its
/// absence is the one signal that the push kept its lease after all.
fn push_kept_lease_against_request(
    force_without_lease: Option<bool>,
    forced_without_lease: Option<bool>,
) -> bool {
    force_without_lease.unwrap_or(false) && forced_without_lease != Some(true)
}

fn log_source_to_proto(log_source: &LogSource) -> proto::GitLogSource {
    proto::GitLogSource {
        source: Some(match log_source {
            LogSource::All => proto::git_log_source::Source::All(proto::GitLogSourceAll {}),
            LogSource::Branch(branch) => proto::git_log_source::Source::Branch(branch.to_string()),
            LogSource::Sha(sha) => proto::git_log_source::Source::Sha(sha.to_string()),
            LogSource::Path(path) => {
                proto::git_log_source::Source::Path(path.as_unix_str().to_owned())
            }
        }),
    }
}

fn log_source_from_proto(log_source: proto::GitLogSource) -> Result<LogSource> {
    match log_source
        .source
        .context("git log source is missing source")?
    {
        proto::git_log_source::Source::All(_) => Ok(LogSource::All),
        proto::git_log_source::Source::Branch(branch) => Ok(LogSource::Branch(branch.into())),
        proto::git_log_source::Source::Sha(sha) => Ok(LogSource::Sha(Oid::from_str(&sha)?)),
        proto::git_log_source::Source::Path(path) => {
            Ok(LogSource::Path(RepoPath::from_proto(&path)?))
        }
    }
}

/// What `SearchCommitsResponse.filters_version` says: this server applies every filter of
/// `SearchCommits` up to here.
const SEARCH_COMMITS_FILTERS_VERSION: u32 = 1;

fn old_server_commit_search_message() -> &'static str {
    i18n::t!(
        "The Zed server on the remote host is too old to search with these conditions. Update Zed on the remote host."
    )
}

fn search_commit_args_to_proto(
    project_id: u64,
    repository_id: u64,
    log_source: &LogSource,
    search_args: &SearchCommitArgs,
) -> proto::SearchCommits {
    let mut authors = search_args.authors.iter().map(|author| author.to_string());
    proto::SearchCommits {
        project_id,
        repository_id,
        log_source: Some(log_source_to_proto(log_source)),
        query: search_args.query.to_string(),
        case_sensitive: search_args.case_sensitive,
        // The first author goes where an older server looks for it, so that a search needing
        // no more than that still works against one.
        author: authors.next(),
        more_authors: authors.collect(),
        only_commits: search_args
            .commits
            .as_ref()
            .map(|commits| proto::SearchCommitsCommitSet {
                shas: commits.iter().map(|commit| commit.to_string()).collect(),
            }),
        branch: search_args.branch.as_ref().map(|branch| branch.to_string()),
        path: search_args.path.as_ref().map(|path| path.to_string()),
        since: search_args.since,
        until: search_args.until,
        change: search_args.change.as_ref().map(|change| change.to_string()),
        merges: match search_args.merges {
            MergeFilter::Include => None,
            MergeFilter::Exclude => Some(proto::SearchCommitsMerges::SearchCommitsExcludeMerges),
            MergeFilter::Only => Some(proto::SearchCommitsMerges::SearchCommitsOnlyMerges),
        }
        .map(|merges| merges as i32),
        sha_prefix: search_args
            .sha_prefix
            .as_ref()
            .map(|sha_prefix| sha_prefix.to_string()),
        search_id: None,
    }
}

/// The search a peer asked for, checked the way a local one is: a filter that arrives must narrow
/// the search, not quietly fall away.
fn search_commit_args_from_proto(request: &proto::SearchCommits) -> Result<SearchCommitArgs> {
    let merges = match request.merges {
        None => MergeFilter::Include,
        Some(merges) => match proto::SearchCommitsMerges::try_from(merges).ok() {
            Some(proto::SearchCommitsMerges::SearchCommitsIncludeMerges) => MergeFilter::Include,
            Some(proto::SearchCommitsMerges::SearchCommitsExcludeMerges) => MergeFilter::Exclude,
            Some(proto::SearchCommitsMerges::SearchCommitsOnlyMerges) => MergeFilter::Only,
            None => bail!(i18n::t!("Cannot read the merge filter ({merges}).")),
        },
    };
    let commits = request
        .only_commits
        .as_ref()
        .map(|commits| {
            commits
                .shas
                .iter()
                .map(|sha| Oid::from_str(sha))
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let search_args = SearchCommitArgs {
        query: SharedString::from(request.query.clone()),
        authors: request
            .author
            .iter()
            .chain(request.more_authors.iter())
            .map(|author| SharedString::from(author.clone()))
            .collect(),
        case_sensitive: request.case_sensitive,
        commits,
        branch: request.branch.clone().map(SharedString::from),
        path: request.path.clone().map(SharedString::from),
        since: request.since,
        until: request.until,
        change: request.change.clone().map(SharedString::from),
        merges,
        sha_prefix: request.sha_prefix.clone().map(SharedString::from),
    };
    search_args.validate()?;
    Ok(search_args)
}

static NEXT_COMMIT_SEARCH_ID: AtomicU64 = AtomicU64::new(0);

fn remote_host_cannot_disable_hooks() -> anyhow::Error {
    anyhow!(i18n::t!(
        "The remote server is too old to run git with hooks turned off, so the AI cannot run git \
         commands there. Update the remote server to let it."
    ))
}

/// What a remote host refusing [`GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY`] means: it is from before the
/// key, and refused the command without running it.
fn remote_hooks_refusal(error: anyhow::Error) -> anyhow::Error {
    if format!("{error:#}").contains(GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY) {
        remote_host_cannot_disable_hooks()
    } else {
        error
    }
}

/// A remote host's answer to a command it was asked to run without hooks, refused unless the host
/// says it did. A host from before the request would not have run it at all (see
/// [`GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY`]); this is the second lock on the same door.
fn remote_output_without_hooks(response: proto::GitRunCommandResponse) -> Result<GitCommandOutput> {
    if !response.hooks_disabled {
        return Err(remote_host_cannot_disable_hooks());
    }
    Ok(GitCommandOutput {
        stdout: response.stdout,
        stderr: response.stderr,
        exit_code: response.exit_code,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum RemoteCommitSearchEnd {
    ServerFinished,
    ReceiverClosed,
}

/// Passes a remote search's results on. When the search uses a filter an older server would drop,
/// an answer from a server that does not say it applies them all is refused, not shown: it would
/// hold more commits than were asked for.
async fn forward_remote_commit_search(
    mut stream: impl Stream<Item = Result<proto::SearchCommitsResponse>> + Unpin,
    requires_filters: bool,
    server_cancels: &AtomicBool,
    request_tx: async_channel::Sender<Oid>,
) -> Result<RemoteCommitSearchEnd> {
    let mut filters_checked = !requires_filters;
    while let Some(response) = stream.next().await {
        let response = response?;
        if response.cancellable == Some(true) {
            server_cancels.store(true, atomic::Ordering::Relaxed);
        }
        if !filters_checked {
            anyhow::ensure!(
                response
                    .filters_version
                    .is_some_and(|version| version >= SEARCH_COMMITS_FILTERS_VERSION),
                old_server_commit_search_message()
            );
            filters_checked = true;
        }
        for sha in &response.shas {
            let oid = Oid::from_str(sha)?;
            if request_tx.send(oid).await.is_err() {
                return Ok(RemoteCommitSearchEnd::ReceiverClosed);
            }
        }
    }
    anyhow::ensure!(filters_checked, old_server_commit_search_message());
    Ok(RemoteCommitSearchEnd::ServerFinished)
}

fn log_order_to_proto(log_order: LogOrder) -> i32 {
    match log_order {
        LogOrder::DateOrder => proto::get_initial_graph_data::LogOrder::DateOrder as i32,
        LogOrder::TopoOrder => proto::get_initial_graph_data::LogOrder::TopoOrder as i32,
        LogOrder::AuthorDateOrder => {
            proto::get_initial_graph_data::LogOrder::AuthorDateOrder as i32
        }
        LogOrder::ReverseChronological => {
            proto::get_initial_graph_data::LogOrder::ReverseChronological as i32
        }
    }
}

fn log_order_from_proto(log_order: proto::get_initial_graph_data::LogOrder) -> LogOrder {
    match log_order {
        proto::get_initial_graph_data::LogOrder::DateOrder => LogOrder::DateOrder,
        proto::get_initial_graph_data::LogOrder::TopoOrder => LogOrder::TopoOrder,
        proto::get_initial_graph_data::LogOrder::AuthorDateOrder => LogOrder::AuthorDateOrder,
        proto::get_initial_graph_data::LogOrder::ReverseChronological => {
            LogOrder::ReverseChronological
        }
    }
}

fn initial_graph_commit_to_proto(commit: &InitialGraphCommitData) -> proto::InitialGraphCommit {
    proto::InitialGraphCommit {
        sha: commit.sha.to_string(),
        parents: commit
            .parents
            .iter()
            .map(|parent| parent.to_string())
            .collect(),
        ref_names: commit
            .ref_names
            .iter()
            .map(|ref_name| ref_name.to_string())
            .collect(),
    }
}

fn initial_graph_commit_from_proto(
    commit: proto::InitialGraphCommit,
) -> Result<Arc<InitialGraphCommitData>> {
    let sha = Oid::from_str(&commit.sha)?;
    let mut parents = SmallVec::with_capacity(commit.parents.len());
    for parent in &commit.parents {
        parents.push(Oid::from_str(parent)?);
    }
    Ok(Arc::new(InitialGraphCommitData {
        sha,
        parents,
        ref_names: commit
            .ref_names
            .into_iter()
            .map(SharedString::from)
            .collect(),
    }))
}

fn commit_data_to_proto(commit: &CommitData) -> proto::CommitData {
    proto::CommitData {
        sha: commit.sha.to_string(),
        parents: commit.parents.iter().map(|p| p.to_string()).collect(),
        author_name: commit.author_name.to_string(),
        author_email: commit.author_email.to_string(),
        commit_timestamp: commit.commit_timestamp,
        subject: commit.subject.to_string(),
        message: commit.message.to_string(),
    }
}

fn commit_data_from_proto(commit: proto::CommitData) -> Result<CommitData> {
    let sha = Oid::from_str(&commit.sha)?;
    let mut parents = SmallVec::with_capacity(commit.parents.len());
    for parent in &commit.parents {
        parents.push(Oid::from_str(parent)?);
    }
    Ok(CommitData {
        sha,
        parents,
        author_name: SharedString::from(commit.author_name),
        author_email: SharedString::from(commit.author_email),
        commit_timestamp: commit.commit_timestamp,
        subject: SharedString::from(commit.subject),
        message: SharedString::from(commit.message),
    })
}

fn branch_to_proto(branch: &git::repository::Branch) -> proto::Branch {
    proto::Branch {
        is_head: branch.is_head,
        ref_name: branch.ref_name.to_string(),
        unix_timestamp: branch
            .most_recent_commit
            .as_ref()
            .map(|commit| commit.commit_timestamp as u64),
        upstream: branch.upstream.as_ref().map(|upstream| proto::GitUpstream {
            ref_name: upstream.ref_name.to_string(),
            tracking: upstream
                .tracking
                .status()
                .map(|upstream| proto::UpstreamTracking {
                    ahead: upstream.ahead as u64,
                    behind: upstream.behind as u64,
                }),
        }),
        most_recent_commit: branch
            .most_recent_commit
            .as_ref()
            .map(|commit| proto::CommitSummary {
                sha: commit.sha.to_string(),
                subject: commit.subject.to_string(),
                commit_timestamp: commit.commit_timestamp,
                author_name: commit.author_name.to_string(),
            }),
    }
}

fn worktree_to_proto(worktree: &git::repository::Worktree) -> proto::Worktree {
    proto::Worktree {
        path: worktree.path.to_string_lossy().to_string(),
        ref_name: worktree
            .ref_name
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        sha: worktree.sha.to_string(),
        is_main: worktree.is_main,
        is_bare: worktree.is_bare,
    }
}

fn proto_to_worktree(proto: &proto::Worktree) -> git::repository::Worktree {
    git::repository::Worktree {
        path: PathBuf::from(proto.path.clone()),
        ref_name: if proto.ref_name.is_empty() {
            None
        } else {
            Some(SharedString::from(&proto.ref_name))
        },
        sha: proto.sha.clone().into(),
        is_main: proto.is_main,
        is_bare: proto.is_bare,
    }
}

fn proto_to_branch(proto: &proto::Branch) -> git::repository::Branch {
    git::repository::Branch {
        is_head: proto.is_head,
        ref_name: proto.ref_name.clone().into(),
        upstream: proto
            .upstream
            .as_ref()
            .map(|upstream| git::repository::Upstream {
                ref_name: upstream.ref_name.to_string().into(),
                tracking: upstream
                    .tracking
                    .as_ref()
                    .map(|tracking| {
                        git::repository::UpstreamTracking::Tracked(UpstreamTrackingStatus {
                            ahead: tracking.ahead as u32,
                            behind: tracking.behind as u32,
                        })
                    })
                    .unwrap_or(git::repository::UpstreamTracking::Gone),
            }),
        most_recent_commit: proto.most_recent_commit.as_ref().map(|commit| {
            git::repository::CommitSummary {
                sha: commit.sha.to_string().into(),
                subject: commit.subject.to_string().into(),
                commit_timestamp: commit.commit_timestamp,
                author_name: commit.author_name.to_string().into(),
                has_parent: true,
            }
        }),
    }
}

fn commit_details_to_proto(commit: &CommitDetails) -> proto::GitCommitDetails {
    proto::GitCommitDetails {
        sha: commit.sha.to_string(),
        message: commit.message.to_string(),
        commit_timestamp: commit.commit_timestamp,
        author_email: commit.author_email.to_string(),
        author_name: commit.author_name.to_string(),
    }
}

fn proto_to_commit_details(proto: &proto::GitCommitDetails) -> CommitDetails {
    CommitDetails {
        sha: proto.sha.clone().into(),
        message: proto.message.clone().into(),
        commit_timestamp: proto.commit_timestamp,
        author_email: proto.author_email.clone().into(),
        author_name: proto.author_name.clone().into(),
    }
}

async fn append_pattern_to_ignore_file(
    fs: Arc<dyn Fs>,
    file_path: PathBuf,
    pattern: String,
) -> Result<()> {
    let existing_content = match fs.load(&file_path).await {
        Ok(content) => content,
        Err(error)
            if error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            String::new()
        }
        Err(error) => {
            return Err(error).with_context(|| format!("loading {}", file_path.display()));
        }
    };

    if existing_content.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }

    let new_content = if existing_content.is_empty() {
        format!("{}\n", pattern)
    } else if existing_content.ends_with('\n') {
        format!("{}{}\n", existing_content, pattern)
    } else {
        format!("{}\n{}\n", existing_content, pattern)
    };

    fs.save(
        &file_path,
        &text::Rope::from(new_content.as_str()),
        text::LineEnding::Unix,
    )
    .await
}

#[cfg(any(test, feature = "test-support"))]
impl Repository {
    pub fn set_branch_list_for_test(&mut self, branches: Vec<Branch>, cx: &mut Context<Self>) {
        self.snapshot.branch_list = branches.into();
        cx.emit(RepositoryEvent::BranchListChanged);
    }

    /// Stands in for a branch listing that read the ref files, which the fake repository does
    /// not have.
    pub fn apply_ref_storage_scan_for_test(
        &mut self,
        ref_storage: RefStorageScan,
        cx: &mut Context<Self>,
    ) {
        self.apply_ref_storage_scan(ref_storage, cx);
    }

    pub fn loaded_commit_data_for_test(&self) -> HashMap<Oid, CommitData> {
        self.commit_data
            .iter()
            .filter_map(|(sha, state)| match state {
                CommitDataState::Loaded(data) => Some((*sha, data.as_ref().clone())),
                CommitDataState::Loading(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Project;
    use fs::{FakeBlobReadGate, FakeFs, Fs};
    use git::repository::{RepoPath, repo_path};
    use gpui::proptest::prelude::*;
    use gpui::{TestAppContext, UpdateGlobal};
    use rand::{SeedableRng, rngs::StdRng};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::{Path, PathBuf};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    #[test]
    fn test_search_commit_args_round_trip_through_proto() {
        let full = SearchCommitArgs {
            query: "直した".into(),
            authors: vec!["山田".into(), "yamada@example.com".into()],
            case_sensitive: true,
            commits: Some(vec![Oid::from_bytes(&[7; 20]).unwrap()]),
            branch: Some("refs/heads/feature".into()),
            path: Some("src/*.rs".into()),
            since: Some(1_700_000_000),
            until: Some(1_800_000_000),
            change: Some("needle".into()),
            merges: MergeFilter::Only,
            sha_prefix: Some("abcd".into()),
        };
        for args in [
            full.clone(),
            SearchCommitArgs {
                query: "fix".into(),
                ..SearchCommitArgs::default()
            },
            SearchCommitArgs {
                merges: MergeFilter::Exclude,
                commits: Some(Vec::new()),
                ..SearchCommitArgs::default()
            },
        ] {
            let request = search_commit_args_to_proto(1, 2, &LogSource::All, &args);
            assert_eq!(search_commit_args_from_proto(&request).unwrap(), args);
        }

        let request = search_commit_args_to_proto(1, 2, &LogSource::All, &full);
        assert_eq!(
            request.author.as_deref(),
            Some("山田"),
            "the first author goes where an older server reads it"
        );
        assert_eq!(request.more_authors, ["yamada@example.com"]);

        // A peer's request is checked like a local search: nothing that would widen it or reach
        // git as an option gets through.
        for request in [
            proto::SearchCommits {
                more_authors: vec![" ".into()],
                ..request.clone()
            },
            proto::SearchCommits {
                branch: Some("--all".into()),
                ..request.clone()
            },
            proto::SearchCommits {
                branch: Some("refs/heads/a..b".into()),
                ..request.clone()
            },
            proto::SearchCommits {
                path: Some("  ".into()),
                ..request.clone()
            },
            proto::SearchCommits {
                sha_prefix: Some("xyz0".into()),
                ..request.clone()
            },
            proto::SearchCommits {
                merges: Some(proto::SearchCommitsMerges::SearchCommitsOnlyMerges as i32 + 1),
                ..request.clone()
            },
            proto::SearchCommits {
                only_commits: Some(proto::SearchCommitsCommitSet {
                    shas: vec!["not a sha".into()],
                }),
                ..request
            },
        ] {
            assert!(
                search_commit_args_from_proto(&request).is_err(),
                "{request:?} should be refused"
            );
        }
    }

    #[gpui::test]
    async fn test_remote_commit_search_refuses_an_old_servers_answer(cx: &mut TestAppContext) {
        let sha = |byte: u8| Oid::from_bytes(&[byte; 20]).unwrap();
        let response = |shas: &[u8], filters_version: Option<u32>| {
            Ok(proto::SearchCommitsResponse {
                shas: shas.iter().map(|byte| sha(*byte).to_string()).collect(),
                filters_version,
                cancellable: None,
            })
        };
        let forward = async |responses: Vec<Result<proto::SearchCommitsResponse>>,
                             requires_filters: bool| {
            let (request_tx, request_rx) = async_channel::unbounded();
            let result = forward_remote_commit_search(
                futures::stream::iter(responses),
                requires_filters,
                &AtomicBool::new(false),
                request_tx,
            )
            .await;
            let mut found = Vec::new();
            while let Ok(oid) = request_rx.try_recv() {
                found.push(oid);
            }
            (result, found)
        };

        let (result, found) = forward(
            vec![
                response(&[], Some(SEARCH_COMMITS_FILTERS_VERSION)),
                response(&[1, 2], None),
            ],
            true,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(found, [sha(1), sha(2)]);

        // An older server drops the filters it does not know and answers with more commits than
        // were asked for; none of them may reach the graph.
        let (result, found) = forward(vec![response(&[1, 2], None)], true).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .starts_with("The Zed server on the remote host is too old"),
        );
        assert!(found.is_empty());
        let (result, _) = forward(Vec::new(), true).await;
        assert!(
            result.is_err(),
            "an older server with nothing to say is not taken for one that found nothing"
        );

        let (result, found) = forward(vec![response(&[1], None)], false).await;
        assert!(
            result.is_ok(),
            "a plain search still works against an older server"
        );
        assert_eq!(found, [sha(1)]);

        let (result, _) = forward(
            vec![
                response(&[], Some(SEARCH_COMMITS_FILTERS_VERSION)),
                Err(anyhow!("ブランチ「x」はありません。")),
            ],
            true,
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "ブランチ「x」はありません。",
            "the server's reason reaches the search box"
        );
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_remote_commit_search_learns_whether_the_server_cancels(cx: &mut TestAppContext) {
        let sha = |byte: u8| Oid::from_bytes(&[byte; 20]).unwrap();
        let response = |shas: &[u8], cancellable: Option<bool>| {
            Ok(proto::SearchCommitsResponse {
                shas: shas.iter().map(|byte| sha(*byte).to_string()).collect(),
                filters_version: Some(SEARCH_COMMITS_FILTERS_VERSION),
                cancellable,
            })
        };

        let server_cancels = AtomicBool::new(false);
        let (request_tx, request_rx) = async_channel::unbounded();
        let end = forward_remote_commit_search(
            futures::stream::iter(vec![response(&[], Some(true)), response(&[1], None)]),
            true,
            &server_cancels,
            request_tx,
        )
        .await;
        assert_eq!(end.unwrap(), RemoteCommitSearchEnd::ServerFinished);
        assert!(server_cancels.load(atomic::Ordering::Relaxed));
        assert_eq!(request_rx.try_recv().ok(), Some(sha(1)));

        // An older server does not say so, and is not sent a message it does not know.
        let server_cancels = AtomicBool::new(false);
        let (request_tx, _request_rx) = async_channel::unbounded();
        forward_remote_commit_search(
            futures::stream::iter(vec![response(&[1], None)]),
            false,
            &server_cancels,
            request_tx,
        )
        .await
        .unwrap();
        assert!(!server_cancels.load(atomic::Ordering::Relaxed));

        // The graph stopped listening while the server was still searching: that is the case
        // the server has to be told about.
        let (request_tx, request_rx) = async_channel::unbounded();
        drop(request_rx);
        let end = forward_remote_commit_search(
            futures::stream::iter(vec![response(&[1, 2], Some(true))]),
            true,
            &AtomicBool::new(false),
            request_tx,
        )
        .await;
        assert_eq!(end.unwrap(), RemoteCommitSearchEnd::ReceiverClosed);
        cx.run_until_parked();
    }

    type TestPasswordPrompt = (
        String,
        oneshot::Sender<EncryptedPassword>,
        oneshot::Receiver<()>,
    );

    fn test_askpass_delegate(
        cx: &mut TestAppContext,
    ) -> (AskPassDelegate, mpsc::UnboundedReceiver<TestPasswordPrompt>) {
        let (prompt_sender, prompt_receiver) = mpsc::unbounded();
        let delegate = AskPassDelegate::new_with_cancellation(
            &mut cx.to_async(),
            move |prompt, response_sender, cancellation, _| {
                prompt_sender
                    .unbounded_send((prompt, response_sender, cancellation))
                    .expect("prompt receiver should remain open");
            },
        );
        (delegate, prompt_receiver)
    }

    /// A force push without a lease has to survive meeting a server that predates the field it is
    /// asked for with, and it may only ever fall back to the safer `--force-with-lease`.
    #[test]
    fn test_force_push_without_lease_over_proto() {
        let (options, force_without_lease) =
            push_options_to_proto(Some(PushOptions::ForceWithoutLease));
        assert_eq!(
            (options, force_without_lease),
            (Some(proto::push::PushOptions::Force as i32), Some(true)),
            "the enum stays at FORCE so that an old server reads something safe out of it"
        );
        assert_eq!(
            push_options_to_proto(Some(PushOptions::Force)),
            (Some(proto::push::PushOptions::Force as i32), None)
        );
        assert_eq!(push_options_to_proto(None), (None, None));

        assert_eq!(
            push_options_from_proto(Some(proto::push::PushOptions::Force), true),
            Some(PushOptions::ForceWithoutLease),
            "a server that reads the flag drops the lease"
        );
        assert_eq!(
            push_options_from_proto(Some(proto::push::PushOptions::Force), false),
            Some(PushOptions::Force),
            "a server that ignores the flag keeps the lease rather than pushing something else"
        );
        assert_eq!(PushOptions::Force.flag(), "--force-with-lease");
        assert_eq!(PushOptions::ForceWithoutLease.flag(), "--force");
        assert_eq!(
            push_options_from_proto(Some(proto::push::PushOptions::ForceWithoutLease), false),
            Some(PushOptions::ForceWithoutLease),
            "clients that predate the flag still get what they asked for"
        );
        assert_eq!(
            push_options_from_proto(Some(proto::push::PushOptions::SetUpstream), true),
            Some(PushOptions::SetUpstream),
            "the flag alone never drops a lease"
        );
        assert_eq!(push_options_from_proto(None, true), None);

        assert!(
            push_kept_lease_against_request(Some(true), None),
            "an answer with no mark on it came from a server that kept the lease"
        );
        assert!(!push_kept_lease_against_request(Some(true), Some(true)));
        assert!(!push_kept_lease_against_request(None, None));

        // Why `handle_push` reads the raw field rather than the generated accessor: an option
        // this build cannot name has to be refused, not read as "push the branch".
        let unknown_option = proto::Push {
            options: Some(proto::push::PushOptions::ForceWithoutLease as i32 + 1),
            ..Default::default()
        };
        assert_eq!(
            unknown_option.options(),
            proto::push::PushOptions::SetUpstream
        );
        assert_eq!(
            proto::push::PushOptions::try_from(unknown_option.options.unwrap_or_default()).ok(),
            None
        );
    }

    #[gpui::test]
    async fn ending_remote_operation_cancels_active_askpass(cx: &mut TestAppContext) {
        let delegates = RemoteAskPassDelegates::default();
        let (delegate, mut prompts) = test_askpass_delegate(cx);
        let operation = RemoteAskPassOperation::new(1, delegate, delegates.clone());
        let password_request = cx.executor().spawn({
            let delegates = delegates.clone();
            async move { request_remote_password(&delegates, 1, "Password:".to_string()).await }
        });
        let (_, response_sender, cancellation) = prompts
            .next()
            .await
            .expect("password prompt should be received");

        drop(operation);

        assert!(cancellation.await.is_err());
        let Err(error) = password_request.await else {
            panic!("password request should be cancelled")
        };
        assert!(error.to_string().contains("remote Git operation ended"));
        assert!(delegates.lock().is_empty());
        drop(response_sender);
    }

    #[gpui::test]
    async fn successful_remote_askpass_allows_another_prompt(cx: &mut TestAppContext) {
        let delegates = RemoteAskPassDelegates::default();
        let (delegate, mut prompts) = test_askpass_delegate(cx);
        let operation = RemoteAskPassOperation::new(1, delegate, delegates.clone());

        for expected_prompt in ["Username:", "Password:"] {
            let password_request =
                cx.executor().spawn({
                    let delegates = delegates.clone();
                    async move {
                        request_remote_password(&delegates, 1, expected_prompt.to_string()).await
                    }
                });
            let (prompt, response_sender, cancellation) = prompts
                .next()
                .await
                .expect("password prompt should be received");
            assert_eq!(prompt, expected_prompt);
            assert!(
                response_sender
                    .send(
                        EncryptedPassword::try_from("secret")
                            .expect("test password should be encryptable")
                    )
                    .is_ok()
            );
            assert!(password_request.await.is_ok());
            assert!(cancellation.await.is_err());
            assert!(
                delegates
                    .lock()
                    .get(&1)
                    .expect("operation should remain registered")
                    .active_request_cancellation
                    .is_none()
            );
        }

        drop(operation);
    }

    #[gpui::test]
    async fn concurrent_remote_askpass_request_is_rejected(cx: &mut TestAppContext) {
        let delegates = RemoteAskPassDelegates::default();
        let (delegate, mut prompts) = test_askpass_delegate(cx);
        let operation = RemoteAskPassOperation::new(1, delegate, delegates.clone());
        let first_request = cx.executor().spawn({
            let delegates = delegates.clone();
            async move { request_remote_password(&delegates, 1, "First:".to_string()).await }
        });
        let (_, response_sender, cancellation) = prompts
            .next()
            .await
            .expect("first password prompt should be received");

        let Err(error) = request_remote_password(&delegates, 1, "Second:".to_string()).await else {
            panic!("concurrent prompt should be rejected");
        };

        assert!(error.to_string().contains("already active"));
        assert!(prompts.next().now_or_never().is_none());

        drop(operation);
        assert!(cancellation.await.is_err());
        assert!(first_request.await.is_err());
        drop(response_sender);
    }

    #[gpui::test]
    async fn remote_askpass_is_rejected_after_operation_ends(cx: &mut TestAppContext) {
        let delegates = RemoteAskPassDelegates::default();
        let (delegate, mut prompts) = test_askpass_delegate(cx);
        let operation = RemoteAskPassOperation::new(1, delegate, delegates.clone());
        drop(operation);

        let Err(error) = request_remote_password(&delegates, 1, "Password:".to_string()).await
        else {
            panic!("prompt should be rejected after operation ends");
        };

        assert!(error.to_string().contains("no longer exists"));
        assert!(prompts.next().now_or_never().is_none());
    }

    #[gpui::test]
    async fn late_remote_askpass_response_does_not_restore_operation(cx: &mut TestAppContext) {
        let delegates = RemoteAskPassDelegates::default();
        let (delegate, mut prompts) = test_askpass_delegate(cx);
        let operation = RemoteAskPassOperation::new(1, delegate, delegates.clone());
        let password_request = cx.executor().spawn({
            let delegates = delegates.clone();
            async move { request_remote_password(&delegates, 1, "Password:".to_string()).await }
        });
        let (_, response_sender, cancellation) = prompts
            .next()
            .await
            .expect("password prompt should be received");

        drop(operation);
        assert!(cancellation.await.is_err());
        assert!(
            response_sender
                .send(
                    EncryptedPassword::try_from("secret")
                        .expect("test password should be encryptable")
                )
                .is_err()
        );
        assert!(password_request.await.is_err());
        assert!(delegates.lock().is_empty());
    }

    #[test]
    fn test_is_submodule_git_dir() {
        assert!(is_submodule_git_dir(Path::new("/Foo/.git/modules/Bar")));
        assert!(is_submodule_git_dir(Path::new(
            "/Foo/.git/modules/nested/modules/Baz"
        )));
        // Bare superprojects keep their git dir in a `*.git` directory.
        assert!(is_submodule_git_dir(Path::new("/Foo/foo.git/modules/Bar")));

        // Not submodules: a normal repo, a linked worktree, and a bare repo.
        assert!(!is_submodule_git_dir(Path::new("/Foo/Bar/.git")));
        assert!(!is_submodule_git_dir(Path::new(
            "/repo/.git/worktrees/feature"
        )));
        assert!(!is_submodule_git_dir(Path::new("/foo/.bare")));
        // A directory literally named `modules` that isn't under a git dir.
        assert!(!is_submodule_git_dir(Path::new("/Foo/modules/Bar")));
    }

    #[gpui::test]
    async fn test_get_permalink_for_file_without_selection(cx: &mut TestAppContext) {
        use util::rel_path::rel_path;

        init_test(cx);
        cx.update(|cx| {
            GitHostingProviderRegistry::default_global(cx);
            git_hosting_providers::init(cx);
        });

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "src": { "main.rs": "fn main() {}\n" },
            }),
        )
        .await;

        let sha = "e6ebe7974deb6bb6cc0e2595c8ec31f0c71084b7";
        fs.set_head_for_repo(
            Path::new("/project/.git"),
            &[("src/main.rs", "fn main() {}\n".into())],
            sha,
        );
        fs.set_remote_for_repo(
            Path::new("/project/.git"),
            "origin",
            "https://github.com/zed-industries/zed.git",
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let project_path = ProjectPath {
            worktree_id,
            path: rel_path("src/main.rs").into(),
        };

        let permalink = project
            .update(cx, |project, cx| {
                project.get_file_permalink(&project_path, cx)
            })
            .await
            .unwrap();

        assert_eq!(
            permalink.as_str(),
            &format!("https://github.com/zed-industries/zed/blob/{sha}/src/main.rs")
        );
        assert!(permalink.fragment().is_none());
    }

    #[gpui::test]
    async fn test_resolve_git_worktree_to_main_repo_ignores_submodule(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/Foo"),
            json!({
                ".git": {
                    "modules": {
                        "Bar": {
                            "HEAD": "ref: refs/heads/main"
                        }
                    }
                },
                "Bar": {
                    ".git": "gitdir: ../.git/modules/Bar\n",
                    "src": { "main.rs": "" }
                }
            }),
        )
        .await;

        // A submodule must not resolve to the superproject's `.git/modules/<name>`;
        // it stays at its own working directory (handled by the caller falling back
        // to the raw path).
        let resolved = resolve_git_worktree_to_main_repo(fs.as_ref(), Path::new("/Foo/Bar")).await;
        assert_eq!(resolved, None);
    }

    #[gpui::test]
    fn test_decode_git_text_windows_1251_one_line_change(cx: &mut TestAppContext) {
        let old_text = "строка один\nстрока два\n";
        let new_text = "строка один\nстрока три\n";
        let (old_bytes, _, _) = encoding_rs::WINDOWS_1251.encode(old_text);
        let (new_bytes, _, _) = encoding_rs::WINDOWS_1251.encode(new_text);

        let decoded_old = decode_git_text(old_bytes.into_owned()).unwrap();
        let decoded_new = decode_git_text(new_bytes.into_owned()).unwrap();
        let buffer = cx.new(|cx| Buffer::local(decoded_new, cx));
        let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
        let diff = cx.new(|cx| BufferDiff::new_with_base_text(&decoded_old, &buffer_snapshot, cx));
        let diff = diff.update(cx, |diff, cx| diff.snapshot(cx));
        let hunks = diff.hunks(&buffer_snapshot).collect::<Vec<_>>();
        let [hunk] = hunks.as_slice() else {
            panic!("expected one modified hunk, got {hunks:?}");
        };

        assert_eq!(hunk.range, text::Point::new(1, 0)..text::Point::new(2, 0));
        assert_eq!(
            hunk.diff_base_byte_range,
            old_text.find("строка два").unwrap()..old_text.len()
        );
    }

    #[gpui::test]
    async fn test_open_uncommitted_diff_skips_symlinks(cx: &mut TestAppContext) {
        use util::rel_path::rel_path;

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "target.txt": "rule one\nrule two\n",
            }),
        )
        .await;
        fs.insert_symlink("/project/agents.md", PathBuf::from("target.txt"))
            .await;

        fs.set_head_and_index_for_repo(
            Path::new("/project/.git"),
            &[
                // git stores the symlink's target path as the blob for `agents.md`
                ("agents.md", "target.txt".into()),
                ("target.txt", "rule one\n".into()),
            ],
        );

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });

        // symlink file should not produce a base diff
        let symlink_buffer = project
            .update(cx, |project, cx| {
                project.open_buffer((worktree_id, rel_path("agents.md")), cx)
            })
            .await
            .unwrap();
        let symlink_diff = project
            .update(cx, |project, cx| {
                project.open_uncommitted_diff(symlink_buffer, cx)
            })
            .await
            .unwrap();
        symlink_diff.read_with(cx, |diff, _| {
            assert!(
                !diff.base_text_exists(),
                "symlinked buffer should not have a git diff base"
            );
        });

        // regular file should still produce a base diff
        let regular_buffer = project
            .update(cx, |project, cx| {
                project.open_buffer((worktree_id, rel_path("target.txt")), cx)
            })
            .await
            .unwrap();
        let regular_diff = project
            .update(cx, |project, cx| {
                project.open_uncommitted_diff(regular_buffer, cx)
            })
            .await
            .unwrap();
        regular_diff.read_with(cx, |diff, _| {
            assert!(
                diff.base_text_exists(),
                "regular file should have a git diff base"
            );
        });
    }

    #[gpui::test]
    async fn test_merge_base_status_uses_worktree_contents(cx: &mut TestAppContext) {
        use util::{path, rel_path::rel_path};

        init_test(cx);

        let project_root = Path::new(path!("/project"));
        let dot_git = project_root.join(".git");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            project_root,
            json!({
                ".git": {},
                "committed.txt": "base\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(&dot_git, &[("committed.txt", "head\n".into())]);
        fs.set_merge_base_content_for_repo(&dot_git, &[("committed.txt", "base\n".into())]);

        let project = Project::test(fs.clone(), [project_root], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let git_store = project.read_with(cx, |project, _| project.git_store().clone());
        let project_path = (worktree_id, rel_path("committed.txt")).into();
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer((worktree_id, rel_path("committed.txt")), cx)
            })
            .await
            .unwrap();
        let buffer_id = buffer.read_with(cx, |buffer, _| buffer.remote_id());

        git_store.read_with(cx, |git_store, cx| {
            assert!(
                git_store
                    .project_path_git_status(&project_path, cx)
                    .is_some_and(|status| status.has_changes())
            );
        });
        cx.update(|cx| {
            SettingsStore::update_global(cx, |settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.git.get_or_insert_default().diff_base =
                        Some(settings::GitDiffBaseSetting::DefaultBranch);
                });
            });
        });
        cx.run_until_parked();

        let display_diff = git_store
            .update(cx, |git_store, cx| {
                git_store.open_display_diff(buffer.clone(), cx)
            })
            .await
            .unwrap();
        display_diff.read_with(cx, |diff, cx| {
            assert_eq!(diff.base_text_string(cx).as_deref(), Some("base\n"));
        });
        git_store.read_with(cx, |git_store, cx| {
            assert_eq!(git_store.display_status_for_buffer_id(buffer_id, cx), None);
            assert_eq!(
                git_store.display_status_for_project_path(&project_path, cx),
                None
            );

            let raw_snapshot = git_store.repo_snapshots(cx).into_values().next().unwrap();
            let display_snapshot = git_store
                .display_repo_snapshots(cx)
                .into_values()
                .next()
                .unwrap();
            assert_eq!(
                raw_snapshot
                    .statuses_by_path
                    .iter()
                    .map(|entry| entry.repo_path.clone())
                    .collect::<Vec<_>>(),
                vec![repo_path("committed.txt")]
            );
            assert_eq!(display_snapshot.statuses_by_path.iter().count(), 0);
        });

        fs.set_merge_base_content_for_repo(&dot_git, &[("committed.txt", "head\n".into())]);
        let repository =
            project.read_with(cx, |project, cx| project.active_repository(cx).unwrap());
        let branches = repository.read_with(cx, |repository, _| {
            repository.snapshot().branch_list.to_vec()
        });
        repository.update(cx, |repository, cx| {
            repository.set_branch_list_for_test(branches, cx)
        });
        cx.run_until_parked();

        git_store.read_with(cx, |git_store, cx| {
            assert!(
                git_store
                    .display_status_for_buffer_id(buffer_id, cx)
                    .is_some_and(|status| status.has_changes())
            );
        });

        let repository_id = repository.read_with(cx, |repository, _| repository.id);
        let display_diff_list = git_store
            .read_with(cx, |git_store, _| {
                git_store.display_diff_for_repo(repository_id)
            })
            .unwrap();
        let weak_display_diff_list = display_diff_list.downgrade();
        drop(display_diff_list);
        drop(display_diff);
        cx.update(|cx| {
            SettingsStore::update_global(cx, |settings, cx| {
                settings.update_user_settings(cx, |settings| {
                    settings.git.get_or_insert_default().diff_base =
                        Some(settings::GitDiffBaseSetting::Head);
                });
            });
        });
        cx.run_until_parked();

        git_store.read_with(cx, |git_store, _| {
            assert!(git_store.display_diff_for_repo(repository_id).is_none());
        });
        assert!(weak_display_diff_list.upgrade().is_none());
    }

    async fn setup_gated_blob_reads(
        cx: &mut TestAppContext,
        count: usize,
    ) -> (FakeBlobReadGate, Entity<Repository>, Vec<git::Oid>) {
        use util::path;

        let project_root = Path::new(path!("/project"));
        let dot_git = project_root.join(".git");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(project_root, json!({ ".git": {} })).await;

        let entries = (0..count)
            .map(|index| (format!("f{index:02}.txt"), format!("blob-{index:02}\n")))
            .collect::<Vec<_>>();
        let entry_refs = entries
            .iter()
            .map(|(name, content)| (name.as_str(), content.clone()))
            .collect::<Vec<_>>();
        let oids = fs.set_merge_base_content_for_repo(&dot_git, &entry_refs);
        let gate = fs.install_blob_read_gate_for_repo(&dot_git);

        let project = Project::test(fs, [project_root], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository =
            project.read_with(cx, |project, cx| project.active_repository(cx).unwrap());
        (gate, repository, oids)
    }

    #[gpui::test]
    async fn test_blob_reads_are_bounded(cx: &mut TestAppContext) {
        init_test(cx);
        let (gate, repository, oids) =
            setup_gated_blob_reads(cx, MAX_CONCURRENT_BLOB_READS + 4).await;

        let reads = oids
            .iter()
            .map(|oid| {
                repository.update(cx, |repository, cx| repository.load_blob_content(*oid, cx))
            })
            .collect::<Vec<_>>();
        cx.run_until_parked();

        assert_eq!(gate.peak_concurrent(), MAX_CONCURRENT_BLOB_READS);
        assert_eq!(gate.waiting(), MAX_CONCURRENT_BLOB_READS);

        gate.open();
        cx.run_until_parked();
        for read in reads {
            read.await.unwrap();
        }
    }

    #[gpui::test]
    async fn test_cancelled_blob_read_releases_permit(cx: &mut TestAppContext) {
        init_test(cx);
        let (gate, repository, oids) =
            setup_gated_blob_reads(cx, MAX_CONCURRENT_BLOB_READS + 1).await;

        let mut holding = oids[..MAX_CONCURRENT_BLOB_READS]
            .iter()
            .map(|oid| {
                repository.update(cx, |repository, cx| repository.load_blob_content(*oid, cx))
            })
            .collect::<Vec<_>>();
        cx.run_until_parked();
        assert_eq!(gate.waiting(), MAX_CONCURRENT_BLOB_READS);

        // One more read can't get a permit, so it never reaches the backend.
        let blocked_oid = oids[MAX_CONCURRENT_BLOB_READS];
        let _blocked = repository.update(cx, |repository, cx| {
            repository.load_blob_content(blocked_oid, cx)
        });
        cx.run_until_parked();
        assert!(!gate.is_waiting(blocked_oid));

        let cancelled_oid = oids[0];
        drop(holding.remove(0));
        cx.run_until_parked();
        assert!(gate.is_waiting(blocked_oid));
        assert!(!gate.is_waiting(cancelled_oid));
        assert_eq!(gate.waiting(), MAX_CONCURRENT_BLOB_READS);
        assert_eq!(gate.peak_concurrent(), MAX_CONCURRENT_BLOB_READS);

        gate.open();
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_append_pattern_to_ignore_file_creates_and_deduplicates(cx: &mut TestAppContext) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let path = PathBuf::from("/root/.gitignore");

        // Appending to a non-existent file creates it with a trailing newline.
        super::append_pattern_to_ignore_file(fs.clone(), path.clone(), "build/".to_string())
            .await
            .unwrap();
        assert_eq!(fs.load(&path).await.unwrap(), "build/\n");

        // Appending the same pattern again is a no-op (deduplication).
        super::append_pattern_to_ignore_file(fs.clone(), path.clone(), "build/".to_string())
            .await
            .unwrap();
        assert_eq!(fs.load(&path).await.unwrap(), "build/\n");

        // Appending a distinct pattern adds it with a trailing newline.
        super::append_pattern_to_ignore_file(fs.clone(), path.clone(), "target/".to_string())
            .await
            .unwrap();
        assert_eq!(fs.load(&path).await.unwrap(), "build/\ntarget/\n");
    }

    #[gpui::test]
    async fn test_append_pattern_adds_newline_before_pattern_when_missing(cx: &mut TestAppContext) {
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let path = PathBuf::from("/root/.gitignore");

        // Pre-populate the file without a trailing newline.
        fs.save(&path, &text::Rope::from("*.log"), text::LineEnding::Unix)
            .await
            .unwrap();

        // The new pattern must be written on its own line.
        super::append_pattern_to_ignore_file(fs.clone(), path.clone(), "build/".to_string())
            .await
            .unwrap();
        assert_eq!(fs.load(&path).await.unwrap(), "*.log\nbuild/\n");
    }

    #[test]
    fn test_new_worktree_path_uses_posix_style_for_remote_paths() {
        let work_dir = Path::new("/home/user/dev/lsp-tests");
        let directory =
            worktrees_directory_for_repo(work_dir, "../worktrees", PathStyle::Unix).unwrap();
        let directory = PathStyle::Unix.join_path(&directory, "nimble-sky").unwrap();
        let path = PathStyle::Unix.join_path(&directory, "lsp-tests").unwrap();

        assert_eq!(
            path,
            PathBuf::from("/home/user/dev/worktrees/lsp-tests/nimble-sky/lsp-tests")
        );
    }

    /// `git` run in `directory` without the machine's global or system configuration, so that
    /// signing, hooks or a default branch set there cannot change the history a test builds.
    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn run_git(directory: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// The closing `rebase --onto` of `rewrite_range` replays every commit after the range, and a
    /// plain rebase flattens a merge commit it meets there. Against a real git: a range below a
    /// merge is refused before anything moves, and a range above one is rewritten with the merge
    /// left a merge.
    #[gpui::test]
    async fn test_rewrite_range_refuses_a_merge_after_the_range(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        // Local, because the rewrite's own git reads the machine's global configuration.
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@zed.dev"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            run_git(&repo_dir, &["config", key, value]);
        }
        let commit = |name: &str| {
            std::fs::write(repo_dir.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            run_git(&repo_dir, &["add", "."]);
            run_git(&repo_dir, &["commit", "-m", name]);
            run_git(&repo_dir, &["rev-parse", "HEAD"])
        };
        commit("initial");
        let below_oldest = commit("A");
        let below_newest = commit("B");
        run_git(&repo_dir, &["checkout", "-b", "side"]);
        let side = commit("S");
        run_git(&repo_dir, &["checkout", "main"]);
        commit("C");
        run_git(&repo_dir, &["merge", "--no-ff", "--no-edit", "side"]);
        let above_oldest = commit("E");
        let above_newest = commit("F");
        let head_before = run_git(&repo_dir, &["rev-parse", "HEAD"]);

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [repo_dir.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the project is a repository")
        });
        assert_eq!(
            repository.read_with(cx, |repository, _| repository.current_branch_name()),
            Some("main".to_string())
        );
        let swapped = |oldest: &str, newest: &str| {
            vec![
                git::repository::RewriteGroup {
                    shas: vec![newest.to_string()],
                    message: None,
                },
                git::repository::RewriteGroup {
                    shas: vec![oldest.to_string()],
                    message: None,
                },
            ]
        };

        // A and B swapped: both are plain commits, but the merge above them would be replayed.
        let error = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    below_oldest.clone(),
                    below_newest.clone(),
                    swapped(&below_oldest, &below_newest),
                    vec![below_oldest.clone(), below_newest.clone()],
                    cx,
                )
            })
            .await
            .unwrap()
            .expect_err("a merge after the range must stop the rewrite");
        assert!(
            error.to_string().contains("merge commit"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            run_git(&repo_dir, &["rev-parse", "HEAD"]),
            head_before,
            "the branch must not move"
        );
        assert_eq!(
            run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert_eq!(
            run_git(
                &repo_dir,
                &[
                    "branch",
                    "--list",
                    &git::repository::rewrite_scratch_branch_name(&below_newest),
                ],
            ),
            "",
            "no scratch branch may be left behind"
        );

        // E and F swapped: the merge is below the range, so it is only the base, never replayed.
        let output = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    above_oldest.clone(),
                    above_newest.clone(),
                    swapped(&above_oldest, &above_newest),
                    vec![above_oldest.clone(), above_newest.clone()],
                    cx,
                )
            })
            .await
            .unwrap()
            .expect("a range after the merge can be rewritten");
        assert!(output.succeeded(), "{}", output.failure_message());
        assert_eq!(
            run_git(&repo_dir, &["log", "--format=%s", "-2"])
                .lines()
                .collect::<Vec<_>>(),
            ["E", "F"]
        );
        assert_eq!(
            run_git(&repo_dir, &["rev-parse", "HEAD~2^2"]),
            side,
            "the merge below the range is still a merge"
        );
    }

    /// Only the commits of `groups` are applied onto the scratch branch, so against a real git: a
    /// list that did not show the range git has (a commit in between it never showed, or one that
    /// is not in the range), a commit from outside the list, a commit applied twice, or a range
    /// whose oldest end is not on the branch is refused before anything moves, while a commit the
    /// list showed and the user left out is dropped.
    #[gpui::test]
    async fn test_rewrite_range_refuses_groups_that_do_not_match_the_range(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@zed.dev"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            run_git(&repo_dir, &["config", key, value]);
        }
        let commit = |name: &str| {
            std::fs::write(repo_dir.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            run_git(&repo_dir, &["add", "."]);
            run_git(&repo_dir, &["commit", "-m", name]);
            run_git(&repo_dir, &["rev-parse", "HEAD"])
        };
        commit("initial");
        let first = commit("A");
        run_git(&repo_dir, &["checkout", "-b", "side"]);
        let elsewhere = commit("X");
        run_git(&repo_dir, &["checkout", "main"]);
        let second = commit("B");
        let third = commit("C");
        let head_before = run_git(&repo_dir, &["rev-parse", "HEAD"]);

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [repo_dir.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the project is a repository")
        });
        let group = |sha: &String| git::repository::RewriteGroup {
            shas: vec![sha.clone()],
            message: None,
        };

        let refusals = [
            // The list showed A and C as if they were next to each other: B, which it never
            // showed, would silently leave the branch.
            (
                first.clone(),
                third.clone(),
                vec![group(&third), group(&first)],
                vec![first.clone(), third.clone()],
                "do not match",
            ),
            // The list showed a commit the range does not have.
            (
                first.clone(),
                second.clone(),
                vec![group(&second), group(&first)],
                vec![first.clone(), second.clone(), elsewhere.clone()],
                "do not match",
            ),
            (
                first.clone(),
                second.clone(),
                vec![group(&second), group(&elsewhere), group(&first)],
                vec![first.clone(), second.clone()],
                "is not in the range to rewrite",
            ),
            (
                first.clone(),
                second.clone(),
                vec![group(&second), group(&first), group(&second)],
                vec![first.clone(), second.clone()],
                "included twice",
            ),
            (
                elsewhere.clone(),
                second.clone(),
                vec![group(&second), group(&elsewhere)],
                vec![elsewhere.clone(), second.clone()],
                "not in the history of the current branch",
            ),
        ];
        for (oldest, newest, groups, expected_shas, reason) in refusals {
            let error = repository
                .update(cx, |repository, cx| {
                    repository.rewrite_range(oldest, newest, groups, expected_shas, cx)
                })
                .await
                .unwrap()
                .expect_err("groups that do not match the range must stop the rewrite");
            assert!(
                error.to_string().contains(reason),
                "expected {reason:?}, got: {error:#}"
            );
            assert_eq!(
                run_git(&repo_dir, &["rev-parse", "HEAD"]),
                head_before,
                "the branch must not move"
            );
            assert_eq!(
                run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
                "main"
            );
        }

        // A shown in the list and left out of the groups is A dropped, which is still allowed.
        let output = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    first.clone(),
                    third.clone(),
                    vec![group(&second), group(&third)],
                    vec![first.clone(), second.clone(), third.clone()],
                    cx,
                )
            })
            .await
            .unwrap()
            .expect("dropping a commit of the range is a rewrite like any other");
        assert!(output.succeeded(), "{}", output.failure_message());
        assert_eq!(
            run_git(&repo_dir, &["log", "--format=%s"])
                .lines()
                .collect::<Vec<_>>(),
            ["C", "B", "initial"]
        );
    }

    /// A file the branch ignores but an older commit still tracks (a local `.env` that was
    /// committed once) must survive a rewrite that starts from before it stopped being tracked:
    /// git counts ignored files as expendable, so a plain `switch` onto the range's base would
    /// overwrite it, and replaying the commit that untracked it would then delete it.
    #[gpui::test]
    async fn test_rewrite_range_keeps_an_ignored_file_the_base_still_tracks(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@zed.dev"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            run_git(&repo_dir, &["config", key, value]);
        }
        let commit = |name: &str| {
            std::fs::write(repo_dir.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            run_git(&repo_dir, &["add", "."]);
            run_git(&repo_dir, &["commit", "-m", name]);
            run_git(&repo_dir, &["rev-parse", "HEAD"])
        };
        std::fs::write(repo_dir.join(".env"), "committed once\n").unwrap();
        commit("initial");
        let first = commit("A");
        let second = commit("B");
        run_git(&repo_dir, &["rm", "--cached", "-q", ".env"]);
        std::fs::write(repo_dir.join(".gitignore"), ".env\n").unwrap();
        run_git(&repo_dir, &["add", ".gitignore"]);
        run_git(&repo_dir, &["commit", "-m", "stop tracking .env"]);
        commit("C");
        std::fs::write(repo_dir.join(".env"), "local secret\n").unwrap();
        let head_before = run_git(&repo_dir, &["rev-parse", "HEAD"]);

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [repo_dir.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the project is a repository")
        });

        let group = |sha: &String| git::repository::RewriteGroup {
            shas: vec![sha.clone()],
            message: None,
        };
        let outcome = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    first.clone(),
                    second.clone(),
                    vec![group(&second), group(&first)],
                    vec![first.clone(), second.clone()],
                    cx,
                )
            })
            .await
            .unwrap();
        // git refusing to switch is the expected way out; what matters is that nothing moved.
        if let Ok(output) = &outcome {
            assert!(
                !output.succeeded(),
                "the rewrite cannot have gone through without touching the ignored file"
            );
        }
        assert_eq!(
            std::fs::read_to_string(repo_dir.join(".env")).unwrap(),
            "local secret\n",
            "the ignored file must be left exactly as it was"
        );
        assert_eq!(run_git(&repo_dir, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(
            run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert_eq!(
            run_git(
                &repo_dir,
                &[
                    "branch",
                    "--list",
                    &git::repository::rewrite_scratch_branch_name(&second),
                ],
            ),
            "",
            "no scratch branch may be left behind"
        );
    }

    /// An unsigned commit on a remote server is recognized by its tag, whatever language the
    /// server wrote the sentence in, and by the sentence from a server without the tag.
    #[test]
    fn test_committed_without_signature_survives_the_remote_trip() {
        use rpc::{ErrorCodeExt as _, ErrorExt as _};
        let relay = |error: anyhow::Error| {
            let mut sent = tag_committed_without_signature(error).to_proto();
            sent.message = "署名なし (a remote server in some other language)".to_string();
            untag_committed_without_signature(rpc::RpcError::from_proto(&sent, "Commit"))
        };

        let received = relay(anyhow::Error::new(
            git::repository::CommittedWithoutSignature,
        ));
        assert!(
            received
                .downcast_ref::<git::repository::CommittedWithoutSignature>()
                .is_some(),
            "{received:#}"
        );
        assert!(git::repository::is_committed_without_signature(&received));

        let other = relay(anyhow!("the commit failed"));
        assert!(!git::repository::is_committed_without_signature(&other));

        for sentence in [
            "The gpg settings of the untrusted repository are not used. Committed without a signature.",
            "未信頼のリポジトリの gpg 設定は使いません。署名せずにコミットしました。",
        ] {
            let from_older_server = untag_committed_without_signature(rpc::RpcError::from_proto(
                &rpc::ErrorCode::Internal
                    .message(sentence.to_string())
                    .to_proto(),
                "Commit",
            ));
            assert!(
                git::repository::is_committed_without_signature(&from_older_server),
                "{sentence}"
            );
        }
    }

    /// An older remote server refusing `ls-files` by name is told apart from any other failure,
    /// and still refuses the rewrite.
    #[test]
    fn test_explain_old_server_refusing_ls_files() {
        let refused =
            explain_old_server_refusing_ls_files(anyhow!("unsupported git command: ls-files"));
        assert_eq!(
            refused.to_string(),
            "The Zed server on the remote host is too old for this rewrite. Update Zed on the remote host."
        );
        for other in [
            "unsupported git command argument: --others",
            "connection lost",
        ] {
            assert_eq!(
                explain_old_server_refusing_ls_files(anyhow!("{other}")).to_string(),
                other
            );
        }
    }

    /// `cherry-pick` and the replay of a rebase overwrite an ignored local file with the version a
    /// commit tracks, without a word, and delete it once a later commit stops tracking it. Against
    /// a real git, every rewrite that would replay such a commit (reordering, dropping, squashing,
    /// rewording, fixup, the merge-request box) is refused before anything moves, naming the file;
    /// one that replays nothing touching it goes through with the file left alone.
    #[gpui::test]
    async fn test_history_rewrites_refuse_to_overwrite_an_ignored_file(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        run_git(&repo_dir, &["init"]);
        run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@zed.dev"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            run_git(&repo_dir, &["config", key, value]);
        }
        let commit = |name: &str| {
            std::fs::write(repo_dir.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            run_git(&repo_dir, &["add", "."]);
            run_git(&repo_dir, &["commit", "-m", name]);
            run_git(&repo_dir, &["rev-parse", "HEAD"])
        };
        let initial = commit("initial");
        let first = commit("A");
        std::fs::write(repo_dir.join(".env"), "committed by mistake\n").unwrap();
        let second = commit("B");
        run_git(&repo_dir, &["rm", "--cached", "-q", ".env"]);
        std::fs::write(repo_dir.join(".gitignore"), ".env\n").unwrap();
        run_git(&repo_dir, &["add", ".gitignore"]);
        run_git(&repo_dir, &["commit", "-m", "stop tracking .env"]);
        let fourth = commit("D");
        let fifth = commit("E");
        std::fs::write(repo_dir.join(".env"), "local secret\n").unwrap();
        let head_before = run_git(&repo_dir, &["rev-parse", "HEAD"]);

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [repo_dir.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the project is a repository")
        });
        let assert_untouched = |operation: &str| {
            assert_eq!(
                std::fs::read_to_string(repo_dir.join(".env")).unwrap(),
                "local secret\n",
                "{operation} must leave the ignored file as it was"
            );
            assert_eq!(
                run_git(&repo_dir, &["rev-parse", "HEAD"]),
                head_before,
                "{operation} must not move the branch"
            );
            assert_eq!(
                run_git(&repo_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
                "main"
            );
            let scratch = format!("{}*", git::repository::REWRITE_SCRATCH_BRANCH_PREFIX);
            assert_eq!(
                run_git(&repo_dir, &["branch", "--list", &scratch]),
                "",
                "{operation} must not leave a scratch branch behind"
            );
        };
        let assert_refused = |operation: &str, error: anyhow::Error| {
            let message = format!("{error:#}");
            assert!(
                message.contains("ignored local files .env would"),
                "{operation}: unexpected error: {message}"
            );
            assert_untouched(operation);
        };
        let group = |sha: &String| git::repository::RewriteGroup {
            shas: vec![sha.clone()],
            message: None,
        };

        // B tracks the file and a later commit stops tracking it, so each of these would replay
        // B's version over the local one and then delete it.
        let error = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    first.clone(),
                    second.clone(),
                    vec![group(&second), group(&first)],
                    vec![first.clone(), second.clone()],
                    cx,
                )
            })
            .await
            .unwrap()
            .expect_err("reordering A and B must be refused");
        assert_refused("rewrite_range", error);

        let error = repository
            .update(cx, |repository, cx| {
                repository.drop_commit(first.clone(), cx)
            })
            .await
            .unwrap()
            .expect_err("dropping A must be refused");
        assert_refused("drop_commit", error);

        let error = repository
            .update(cx, |repository, cx| {
                repository.squash_range(first.clone(), second.clone(), "A と B".to_string(), cx)
            })
            .await
            .unwrap()
            .expect_err("squashing A and B must be refused");
        assert_refused("squash_range", error);

        let error = repository
            .update(cx, |repository, cx| {
                repository.reword_commit(first.clone(), "A (reworded)".to_string(), cx)
            })
            .await
            .unwrap()
            .expect_err("rewording A must be refused");
        assert_refused("reword_commit", error);

        std::fs::write(repo_dir.join("A.txt"), "A, amended\n").unwrap();
        run_git(&repo_dir, &["add", "A.txt"]);
        let error = repository
            .update(cx, |repository, cx| {
                repository.fixup_commit(first.clone(), git::repository::FixupChanges::Staged, cx)
            })
            .await
            .unwrap()
            .expect_err("a fixup of A must be refused");
        assert_refused("fixup_commit", error);
        assert_eq!(
            run_git(&repo_dir, &["diff", "--cached", "--name-only"]),
            "A.txt",
            "the refused fixup leaves the staged change staged"
        );
        run_git(&repo_dir, &["reset", "-q", "--hard"]);

        let merge_request = |shas: &[&String], branch: &str| MergeRequestBuild {
            base: initial.clone(),
            shas_oldest_first: shas.iter().map(|sha| sha.to_string()).collect(),
            merge_request_branch: branch.to_string(),
            message: "MR".to_string(),
            recreate: false,
            origin_branch: "main".to_string(),
            scratch_branch: git::repository::merge_request_scratch_branch_name(
                shas.last().map(|sha| sha.as_str()).unwrap_or_default(),
            ),
        };
        let outcome = repository
            .update(cx, |repository, cx| {
                repository.build_merge_request_branch(merge_request(&[&second], "mr/b"), cx)
            })
            .await
            .unwrap()
            .unwrap();
        let MergeRequestBuildOutcome::Refused {
            reason,
            left_on_scratch_branch: false,
        } = outcome
        else {
            panic!("a box holding B must be refused, got {outcome:?}");
        };
        assert!(
            reason.contains("ignored local files .env would"),
            "{reason}"
        );
        assert_untouched("build_merge_request_branch");
        assert_eq!(run_git(&repo_dir, &["branch", "--list", "mr/b"]), "");

        // Neither D nor E touches the file: the ignored file being there changes nothing.
        let outcome = repository
            .update(cx, |repository, cx| {
                repository.build_merge_request_branch(merge_request(&[&fourth], "mr/d"), cx)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(outcome, MergeRequestBuildOutcome::Created { .. }),
            "{outcome:?}"
        );
        assert_untouched("a merge request that does not touch the file");

        let output = repository
            .update(cx, |repository, cx| {
                repository.rewrite_range(
                    fourth.clone(),
                    fifth.clone(),
                    vec![group(&fifth), group(&fourth)],
                    vec![fourth.clone(), fifth.clone()],
                    cx,
                )
            })
            .await
            .unwrap()
            .expect("a rewrite that does not touch the file goes through");
        assert!(output.succeeded(), "{}", output.failure_message());
        assert_eq!(
            run_git(&repo_dir, &["log", "--format=%s", "-2"])
                .lines()
                .collect::<Vec<_>>(),
            ["D", "E"]
        );
        assert_eq!(
            std::fs::read_to_string(repo_dir.join(".env")).unwrap(),
            "local secret\n"
        );
    }

    fn verify_invariants(repository: &Repository) -> anyhow::Result<()> {
        match &repository.commit_data_handler {
            CommitDataHandlerState::Open(handler) => {
                verify_loading_entries_are_pending(repository, handler)?;
                verify_await_result_loading_entries_have_completion_senders(repository, handler)?;
                verify_pending_requests_are_loading(repository, handler)?;
                verify_completion_senders_are_await_result_loading(repository, handler)?;
                verify_completion_senders_are_pending(handler)?;
                verify_non_await_result_loading_entries_have_no_completion_sender(
                    repository, handler,
                )?;
                verify_loaded_entries_are_not_pending(repository, handler)?;
                verify_loaded_entries_have_no_completion_sender(repository, handler)?;
            }
            CommitDataHandlerState::Closed => {
                verify_closed_handler_invariants(repository)?;
            }
        }

        Ok(())
    }

    fn verify_loading_entries_are_pending(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            if matches!(state, CommitDataState::Loading(_)) {
                anyhow::ensure!(
                    handler.pending_requests.contains(sha),
                    "loading commit data for {sha} must be tracked in pending_requests"
                );
            }
        }

        Ok(())
    }

    fn verify_await_result_loading_entries_have_completion_senders(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            if matches!(state, CommitDataState::Loading(Some(_))) {
                anyhow::ensure!(
                    handler.completion_senders.contains_key(sha),
                    "await-result loading commit data for {sha} must have a completion sender"
                );
            }
        }

        Ok(())
    }

    fn verify_pending_requests_are_loading(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for sha in &handler.pending_requests {
            anyhow::ensure!(
                matches!(
                    repository.commit_data.get(sha),
                    Some(CommitDataState::Loading(_))
                ),
                "pending request for {sha} must correspond to loading commit data"
            );
        }

        Ok(())
    }

    fn verify_completion_senders_are_await_result_loading(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for sha in handler.completion_senders.keys() {
            anyhow::ensure!(
                matches!(
                    repository.commit_data.get(sha),
                    Some(CommitDataState::Loading(Some(_)))
                ),
                "completion sender for {sha} must correspond to await-result loading commit data"
            );
        }

        Ok(())
    }

    fn verify_completion_senders_are_pending(handler: &CommitDataHandler) -> anyhow::Result<()> {
        for sha in handler.completion_senders.keys() {
            anyhow::ensure!(
                handler.pending_requests.contains(sha),
                "completion sender for {sha} must also be tracked as pending"
            );
        }

        Ok(())
    }

    fn verify_non_await_result_loading_entries_have_no_completion_sender(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            if matches!(state, CommitDataState::Loading(None)) {
                anyhow::ensure!(
                    !handler.completion_senders.contains_key(sha),
                    "non-await-result loading commit data for {sha} must not have a completion sender"
                );
            }
        }

        Ok(())
    }

    fn verify_loaded_entries_are_not_pending(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            if matches!(state, CommitDataState::Loaded(_)) {
                anyhow::ensure!(
                    !handler.pending_requests.contains(sha),
                    "loaded commit data for {sha} must not still be pending"
                );
            }
        }

        Ok(())
    }

    fn verify_loaded_entries_have_no_completion_sender(
        repository: &Repository,
        handler: &CommitDataHandler,
    ) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            if matches!(state, CommitDataState::Loaded(_)) {
                anyhow::ensure!(
                    !handler.completion_senders.contains_key(sha),
                    "loaded commit data for {sha} must not keep a completion sender"
                );
            }
        }

        Ok(())
    }

    fn verify_closed_handler_invariants(repository: &Repository) -> anyhow::Result<()> {
        for (sha, state) in &repository.commit_data {
            anyhow::ensure!(
                !matches!(state, CommitDataState::Loading(_)),
                "closed handler must not keep loading commit data for {sha}"
            );
        }

        Ok(())
    }

    #[gpui::property_test(config = ProptestConfig {
        cases: 20,
        ..Default::default()
    })]
    async fn test_commit_data_random_invariants(
        #[strategy = any::<u64>()] seed: u64,
        #[strategy = gpui::proptest::collection::vec(0usize..2000, 1..200)] commit_indexes: Vec<
            usize,
        >,
        #[strategy = gpui::proptest::collection::vec(any::<bool>(), 1..200)] await_results: Vec<
            bool,
        >,
        #[strategy = gpui::proptest::collection::vec(0usize..2000, 0..200)] failing_commit_indexes: Vec<
            usize,
        >,
        #[strategy = gpui::proptest::collection::vec(0usize..2000, 0..200)] missing_commit_indexes: Vec<
            usize,
        >,
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let mut rng = StdRng::seed_from_u64(seed);

        let commit_shas = (0..2000).map(|_| Oid::random(&mut rng)).collect::<Vec<_>>();
        let failing_shas = failing_commit_indexes
            .into_iter()
            .map(|index| commit_shas[index % commit_shas.len()])
            .collect::<HashSet<_>>();
        let missing_shas = missing_commit_indexes
            .into_iter()
            .map(|index| commit_shas[index % commit_shas.len()])
            .collect::<HashSet<_>>();
        let commit_data = commit_shas
            .iter()
            .filter(|sha| !missing_shas.contains(sha))
            .map(|sha| {
                (
                    CommitData {
                        sha: *sha,
                        parents: SmallVec::new(),
                        author_name: SharedString::from(format!("Author {sha}")),
                        author_email: SharedString::from(format!("{sha}@example.com")),
                        commit_timestamp: rng.random_range(0..10_000),
                        subject: SharedString::from(format!("Subject {sha}")),
                        message: SharedString::from(format!("Subject {sha}\n\nBody for {sha}")),
                    },
                    failing_shas.contains(sha),
                )
            })
            .collect::<Vec<_>>();
        let expected_loaded_shas = commit_indexes
            .iter()
            .map(|index| commit_shas[index % commit_shas.len()])
            .filter(|sha| !failing_shas.contains(sha) && !missing_shas.contains(sha))
            .collect::<HashSet<_>>();

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new("/project"),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_commit_data(Path::new("/project/.git"), commit_data);

        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });

        cx.update(|cx| {
            cx.observe(&repository, |repo, cx| {
                verify_invariants(repo.read(cx))
                    .context("Invariant weren't held after a cx.notify")
                    .unwrap();
            })
        })
        .detach();

        let mut next_step = 0;
        while next_step < commit_indexes.len() {
            let remaining_steps = commit_indexes.len() - next_step;
            let chunk_size = rng.random_range(1..=remaining_steps.min(16));
            let chunk_end = next_step + chunk_size;

            for step in next_step..chunk_end {
                let sha = commit_shas[commit_indexes[step] % commit_shas.len()];
                let await_result = await_results[step % await_results.len()];

                repository.update(cx, |repository, cx| {
                    repository.fetch_commit_data(sha, await_result, cx);
                    verify_invariants(repository)
                        .with_context(|| {
                            format!(
                                "commit data invariant violation after step {} for sha {}",
                                step + 1,
                                sha,
                            )
                        })
                        .unwrap();
                });
            }

            cx.run_until_parked();
            repository.read_with(cx, |repository, _cx| {
                verify_invariants(repository)
                    .with_context(|| {
                        format!(
                            "commit data invariant violation after draining through step {}",
                            chunk_end,
                        )
                    })
                    .unwrap();
            });

            next_step = chunk_end;
        }

        cx.run_until_parked();
        repository.read_with(cx, |repository, _cx| {
            verify_invariants(repository)
                .with_context(|| "commit data invariant violation after final drain".to_string())
                .unwrap();

            let loaded_shas = repository
                .commit_data
                .iter()
                .filter_map(|(sha, state)| match state {
                    CommitDataState::Loaded(_) => Some(*sha),
                    CommitDataState::Loading(_) => None,
                })
                .collect::<HashSet<_>>();
            let missing_loaded_shas = expected_loaded_shas
                .difference(&loaded_shas)
                .copied()
                .collect::<Vec<_>>();
            let unexpected_loaded_shas = loaded_shas
                .difference(&expected_loaded_shas)
                .copied()
                .collect::<Vec<_>>();
            assert!(
                missing_loaded_shas.is_empty() && unexpected_loaded_shas.is_empty(),
                "loaded commit data SHAs after final drain did not match expectation. missing: {:?}, unexpected: {:?}",
                missing_loaded_shas,
                unexpected_loaded_shas,
            );
        });
    }

    fn repo_paths(paths: &[&str]) -> Vec<RepoPath> {
        paths.iter().map(repo_path).collect()
    }

    #[test]
    fn coalesce_repo_paths_keeps_root_only() {
        let coalesced = GitStore::coalesce_repo_paths(repo_paths(&["", "src", "src/lib.rs"]));

        assert_eq!(coalesced, repo_paths(&[""]));
    }

    #[test]
    fn coalesce_repo_paths_keeps_existing_ancestors() {
        let coalesced = GitStore::coalesce_repo_paths(repo_paths(&[
            "src",
            "src/lib.rs",
            "src/nested/file.rs",
            "tests/test.rs",
        ]));

        assert_eq!(coalesced, repo_paths(&["src", "tests/test.rs"]));
    }

    #[test]
    fn coalesce_repo_paths_does_not_invent_missing_parents() {
        let coalesced = GitStore::coalesce_repo_paths(repo_paths(&[
            "submodule/a.txt",
            "submodule/nested/b.txt",
            "top_level.rs",
        ]));

        assert_eq!(
            coalesced,
            repo_paths(&["submodule/a.txt", "submodule/nested/b.txt", "top_level.rs"])
        );
    }

    #[test]
    fn test_snapshot_scan_for_git_dir_change() {
        assert_eq!(
            SnapshotScan::for_git_dir_change(Some(GitDirChange::Refs)),
            SnapshotScan {
                refs: true,
                statuses: false
            }
        );
        assert_eq!(
            SnapshotScan::for_git_dir_change(Some(GitDirChange::Index)),
            SnapshotScan {
                refs: false,
                statuses: true
            }
        );
        assert_eq!(
            SnapshotScan::for_git_dir_change(Some(GitDirChange::Unknown)),
            SnapshotScan::FULL
        );
        assert_eq!(
            SnapshotScan::for_git_dir_change(None),
            SnapshotScan::FULL,
            "an unclassified change must always fall back to the full scan"
        );
        assert_eq!(
            SnapshotScan::for_git_dir_change(Some(GitDirChange::Refs))
                .union(SnapshotScan::for_git_dir_change(Some(GitDirChange::Index))),
            SnapshotScan::FULL
        );
    }

    #[gpui::test]
    async fn test_refs_only_git_dir_event_skips_the_status_pass(cx: &mut TestAppContext) {
        use util::path;

        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(path!("/project")),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            Path::new(path!("/project/.git")),
            &[("file.txt", "content".into())],
        );

        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        repository.read_with(cx, |repository, _| {
            let statuses = repository.snapshot.status().collect::<Vec<_>>();
            assert_eq!(
                statuses.len(),
                0,
                "unexpected initial statuses: {statuses:?}"
            );
        });

        // Stage a change without letting the fake repository announce it, so
        // that the only thing the worktree sees is the ref update below.
        fs.with_git_state(Path::new(path!("/project/.git")), false, |state| {
            state
                .index_contents
                .insert(repo_path("file.txt"), b"staged".to_vec());
            state.branches.insert("feature".into());
        })
        .unwrap();
        fs.emit_fs_event(
            Path::new(path!("/project/.git/refs/heads/feature")),
            Some(fs::PathEventKind::Created),
        );
        cx.run_until_parked();
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        repository.read_with(cx, |repository, _| {
            assert_eq!(
                repository.snapshot.status().count(),
                0,
                "a ref-only `.git` event must not run `git status`"
            );
            assert!(
                repository
                    .snapshot
                    .branch_list
                    .iter()
                    .any(|branch| branch.name() == "feature"),
                "a ref-only `.git` event must still refresh the branch list"
            );
        });

        // An index event, by contrast, has to pick the staged change up.
        fs.emit_fs_event(
            Path::new(path!("/project/.git/index")),
            Some(fs::PathEventKind::Changed),
        );
        cx.run_until_parked();
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;

        repository.read_with(cx, |repository, _| {
            assert_eq!(
                repository.snapshot.status().count(),
                1,
                "an index `.git` event must run `git status`"
            );
        });
    }

    async fn fake_repository(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeFs>,
        Entity<Repository>,
        Arc<Mutex<Vec<RepositoryEvent>>>,
    ) {
        use util::path;

        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            Path::new(path!("/project")),
            json!({
                ".git": {},
                "file.txt": "content",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [Path::new(path!("/project"))], cx).await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("should have a repository")
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&repository, move |_, event: &RepositoryEvent, _| {
                events.lock().push(event.clone());
            })
            .detach();
        });
        (fs, repository, events)
    }

    /// ADR-F90: an agent's command goes to the repository's hook-free entry point, and a remote
    /// host is believed only when it says it turned hooks off.
    #[gpui::test]
    async fn test_run_git_command_without_hooks(cx: &mut TestAppContext) {
        let (fs, repository, _) = fake_repository(cx).await;
        let dot_git = Path::new(util::path!("/project/.git"));
        fs.with_git_state(dot_git, false, |state| {
            state.command_outputs.insert(
                "log -1".to_string(),
                GitCommandOutput {
                    stdout: "commit\n".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            );
        })
        .unwrap();

        let output = repository
            .update(cx, |repository, _| {
                repository.run_git_command_without_hooks(
                    vec!["log".into(), "-1".into()],
                    None,
                    HashMap::default(),
                )
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.stdout, "commit\n");
        let without_hooks = fs
            .with_git_state(dot_git, false, |state| {
                state.run_command_without_hooks.last().copied()
            })
            .unwrap();
        assert_eq!(without_hooks, Some(true));

        // What a host from before the key does with it: the request reaches
        // `run_git_command_with_env`, whose environment check refuses it before git runs.
        let runs_before = fs
            .with_git_state(dot_git, false, |state| state.run_command_log.len())
            .unwrap();
        let error = repository
            .update(cx, |repository, _| {
                repository.run_git_command_with_env(
                    vec!["log".into(), "-1".into()],
                    None,
                    HashMap::from_iter([(
                        GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY.to_string(),
                        String::new(),
                    )]),
                )
            })
            .await
            .unwrap()
            .expect_err("an older host refuses the key");
        let runs_after = fs
            .with_git_state(dot_git, false, |state| state.run_command_log.len())
            .unwrap();
        assert_eq!(runs_before, runs_after, "nothing ran");
        let error = remote_hooks_refusal(error);
        assert!(
            error.to_string().contains("too old"),
            "the refusal is told apart: {error:#}"
        );
        let other = remote_hooks_refusal(anyhow!("connection lost"));
        assert_eq!(other.to_string(), "connection lost");

        // A host that answers without saying so is not believed.
        let response = |hooks_disabled| proto::GitRunCommandResponse {
            stdout: "out".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            hooks_disabled,
        };
        let error =
            remote_output_without_hooks(response(false)).expect_err("an answer from an older host");
        assert!(error.to_string().contains("too old"), "{error:#}");
        assert_eq!(
            remote_output_without_hooks(response(true)).unwrap().stdout,
            "out"
        );
    }

    fn describe_invocations(fs: &FakeFs) -> Vec<String> {
        fs.with_git_state(Path::new(util::path!("/project/.git")), false, |state| {
            state
                .run_command_log
                .iter()
                .filter(|(args, _)| args.first().map(String::as_str) == Some("describe"))
                .filter_map(|(args, _)| args.last().cloned())
                .collect()
        })
        .unwrap()
    }

    #[gpui::test]
    async fn test_latest_describe_runs_only_the_newest_request(cx: &mut TestAppContext) {
        let (fs, repository, _) = fake_repository(cx).await;
        fs.with_git_state(Path::new(util::path!("/project/.git")), false, |state| {
            for committish in ["first", "second", "third"] {
                state.command_outputs.insert(
                    format!("describe --tags --long {committish}"),
                    GitCommandOutput {
                        stdout: format!("v1-0-g{committish}\n"),
                        stderr: String::new(),
                        exit_code: Some(0),
                    },
                );
            }
        })
        .unwrap();

        let requester = EntityId::from(1u64);
        let (first, second, third) = repository.update(cx, |repository, _| {
            (
                repository.latest_describe("first".into(), requester),
                repository.latest_describe("second".into(), requester),
                repository.latest_describe("third".into(), requester),
            )
        });
        cx.run_until_parked();

        assert_eq!(describe_invocations(&fs), ["third"]);
        assert!(first.await.is_err(), "superseded requests are dropped");
        assert!(second.await.is_err());
        assert_eq!(
            third
                .await
                .unwrap()
                .unwrap()
                .map(|distance| distance.short_sha),
            Some("third".to_string())
        );

        // Another view's request, and plain `describe`, are never dropped for it.
        let (other_view, plain) = repository.update(cx, |repository, _| {
            (
                repository.latest_describe("first".into(), requester),
                repository.describe("second".into()),
            )
        });
        let other_requester = repository.update(cx, |repository, _| {
            repository.latest_describe("third".into(), EntityId::from(2u64))
        });
        cx.run_until_parked();
        assert_eq!(
            describe_invocations(&fs),
            ["third", "first", "second", "third"]
        );
        assert!(other_view.await.unwrap().is_ok());
        assert!(plain.await.unwrap().is_ok());
        assert!(other_requester.await.unwrap().is_ok());
    }

    #[gpui::test]
    async fn test_tags_changed_follows_the_tags_fingerprint(cx: &mut TestAppContext) {
        let (_fs, repository, events) = fake_repository(cx).await;
        let tag_events = || {
            events
                .lock()
                .drain(..)
                .filter(|event| *event == RepositoryEvent::TagsChanged)
                .count()
        };
        let scan = |tags_fingerprint: Option<u64>, loose_ref_count: Option<usize>| RefStorageScan {
            loose_refs: loose_ref_count.map(|count| LooseRefCount {
                count,
                complete: true,
            }),
            tags_fingerprint,
        };
        tag_events();

        // The fake repository reads no ref files, so its scans never announce anything.
        repository.read_with(cx, |repository, _| {
            assert_eq!(repository.snapshot.tags_fingerprint, None);
            assert_eq!(repository.snapshot.loose_refs, None);
        });

        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(scan(Some(1), Some(1_500)), cx)
        });
        assert_eq!(tag_events(), 1, "the first fingerprint");
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(scan(Some(1), Some(10)), cx)
        });
        assert_eq!(tag_events(), 0, "a ref change that left the tags alone");
        repository.read_with(cx, |repository, _| {
            assert_eq!(
                repository.snapshot.loose_refs,
                Some(LooseRefCount {
                    count: 10,
                    complete: true,
                })
            );
        });
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(scan(Some(2), Some(10)), cx)
        });
        assert_eq!(tag_events(), 1, "a tag change");
        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(scan(None, None), cx)
        });
        assert_eq!(
            tag_events(),
            0,
            "nothing is announced while the tags cannot be told"
        );
        repository.read_with(cx, |repository, _| {
            assert_eq!(repository.snapshot.tags_fingerprint, None);
        });
    }

    /// What a host sends about its refs arrives on the other end of an SSH project, and a host
    /// too old to send it leaves the other end not knowing rather than guessing.
    #[gpui::test]
    async fn test_ref_storage_travels_in_repository_updates(cx: &mut TestAppContext) {
        let (_fs, repository, events) = fake_repository(cx).await;
        let host_update = repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(
                RefStorageScan {
                    loose_refs: Some(LooseRefCount {
                        count: 10_000,
                        complete: false,
                    }),
                    tags_fingerprint: Some(7),
                },
                cx,
            );
            repository.snapshot.initial_update(1)
        });
        assert_eq!(host_update.tags_fingerprint, Some(7));
        assert_eq!(host_update.loose_ref_count, Some(10_000));
        assert!(host_update.loose_ref_count_is_lower_bound);

        repository.update(cx, |repository, cx| {
            repository.apply_ref_storage_scan_for_test(RefStorageScan::default(), cx);
        });
        events.lock().clear();
        repository.update(cx, |repository, cx| {
            repository
                .apply_remote_update(host_update.clone(), cx)
                .unwrap();
        });
        repository.read_with(cx, |repository, _| {
            assert_eq!(repository.snapshot.tags_fingerprint, Some(7));
            assert_eq!(
                repository.snapshot.loose_refs,
                Some(LooseRefCount {
                    count: 10_000,
                    complete: false,
                })
            );
        });
        assert!(events.lock().contains(&RepositoryEvent::TagsChanged));

        events.lock().clear();
        repository.update(cx, |repository, cx| {
            repository
                .apply_remote_update(
                    proto::UpdateRepository {
                        tags_fingerprint: None,
                        loose_ref_count: None,
                        loose_ref_count_is_lower_bound: false,
                        ..host_update
                    },
                    cx,
                )
                .unwrap();
        });
        repository.read_with(cx, |repository, _| {
            assert_eq!(repository.snapshot.tags_fingerprint, None);
            assert_eq!(repository.snapshot.loose_refs, None);
        });
        assert!(!events.lock().contains(&RepositoryEvent::TagsChanged));
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn installed_git(directory: &Path, arguments: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("failed to run git")
    }

    #[track_caller]
    fn installed_git_output(directory: &Path, arguments: &[&str]) -> String {
        let output = installed_git(directory, arguments);
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A repository on `dev` with four commits on top of `main` (A, a commit left out of the
    /// merge request, B and C), each adding its own file.
    fn merge_request_repository(
        directory: &Path,
    ) -> std::collections::BTreeMap<&'static str, String> {
        std::fs::create_dir_all(directory).unwrap();
        installed_git_output(directory, &["init"]);
        installed_git_output(directory, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        // The job runs git the way Zed does, with whatever configuration the machine has, so
        // everything that would get in its way is pinned in the repository itself.
        for (key, value) in [
            ("user.name", "test"),
            ("user.email", "test@zed.dev"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            installed_git_output(directory, &["config", key, value]);
        }
        let mut shas = std::collections::BTreeMap::new();
        let mut commit = |name: &'static str, file: &str, contents: &str| {
            std::fs::write(directory.join(file), contents).unwrap();
            installed_git_output(directory, &["add", "."]);
            installed_git_output(directory, &["commit", "-m", name]);
            shas.insert(
                name,
                installed_git_output(directory, &["rev-parse", "HEAD"]),
            );
        };
        commit("base", "base.txt", "base\n");
        installed_git_output(directory, &["checkout", "-b", "dev"]);
        commit("A", "a.txt", "a\n");
        commit("left-out", "left-out.txt", "left out\n");
        commit("B", "b.txt", "b\n");
        commit("C", "c.txt", "c\n");
        shas
    }

    fn merge_request_build(
        base: &str,
        shas: &[&String],
        merge_request_branch: &str,
        recreate: bool,
    ) -> MergeRequestBuild {
        let newest = shas.last().map(|sha| sha.as_str()).unwrap_or_default();
        MergeRequestBuild {
            base: base.to_string(),
            shas_oldest_first: shas.iter().map(|sha| sha.to_string()).collect(),
            merge_request_branch: merge_request_branch.to_string(),
            message: "MR にまとめる".to_string(),
            recreate,
            origin_branch: "dev".to_string(),
            scratch_branch: git::repository::merge_request_scratch_branch_name(newest),
        }
    }

    async fn run_merge_request_build(
        repository: &Entity<Repository>,
        build: MergeRequestBuild,
        cx: &mut TestAppContext,
    ) -> MergeRequestBuildOutcome {
        repository
            .update(cx, |repository, cx| {
                repository.build_merge_request_branch(build, cx)
            })
            .await
            .expect("the job should run")
            .expect("the build should come back with an outcome")
    }

    fn scratch_branches(directory: &Path) -> String {
        let pattern = format!("{}*", git::repository::REWRITE_SCRATCH_BRANCH_PREFIX);
        installed_git_output(directory, &["branch", "--list", &pattern])
    }

    /// The merge-request box's build against the installed git, end to end through the
    /// repository's job queue: one commit on the base with exactly the picked changes, the user
    /// back on their branch, nothing left behind; an existing branch only moved when asked (and
    /// where it was reported); a dirty tree and a detached HEAD refused untouched; a conflict
    /// either abandoned back to the start or continued and finished.
    #[gpui::test]
    async fn test_merge_request_build_on_installed_git(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let directory = temp_dir.path().join("repo");
        let shas = merge_request_repository(&directory);
        let base = shas["base"].clone();

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [directory.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the temporary repository should be found")
        });

        // (a)-(c): a new branch.
        let build = merge_request_build(
            "main",
            &[&shas["A"], &shas["B"], &shas["C"]],
            "mr/dev",
            false,
        );
        let scratch = build.scratch_branch.clone();
        let outcome = run_merge_request_build(&repository, build, cx).await;
        let MergeRequestBuildOutcome::Created {
            merge_request_sha,
            previous_sha: None,
            return_failure: None,
        } = outcome
        else {
            panic!("the build should have worked, got {outcome:?}");
        };
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "mr/dev"]),
            merge_request_sha
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "mr/dev^"]),
            base,
            "the merge request is one commit on top of the base"
        );
        assert_eq!(
            installed_git_output(&directory, &["log", "-1", "--format=%s", "mr/dev"]),
            "MR にまとめる"
        );
        assert_eq!(
            installed_git_output(&directory, &["diff", "--name-only", &base, "mr/dev"]),
            "a.txt\nb.txt\nc.txt",
            "the picked commits' changes and nothing of the one left out"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "dev"]),
            shas["C"],
            "the development branch did not move"
        );
        assert_eq!(scratch_branches(&directory), "", "{scratch} should be gone");

        // (d): the existing branch is refused without the checkbox, and moved with it.
        std::fs::write(directory.join("d.txt"), "d\n").unwrap();
        installed_git_output(&directory, &["add", "."]);
        installed_git_output(&directory, &["commit", "-m", "D"]);
        let d = installed_git_output(&directory, &["rev-parse", "HEAD"]);
        let picked = [&shas["A"], &shas["B"], &shas["C"], &d];
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &picked, "mr/dev", false),
            cx,
        )
        .await;
        assert!(
            matches!(&outcome, MergeRequestBuildOutcome::Refused { left_on_scratch_branch: false, reason } if reason.contains("already exists")),
            "{outcome:?}"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "mr/dev"]),
            merge_request_sha
        );
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &picked, "mr/dev", true),
            cx,
        )
        .await;
        let MergeRequestBuildOutcome::Created {
            merge_request_sha: recreated_sha,
            previous_sha: Some(previous_sha),
            return_failure: None,
        } = outcome
        else {
            panic!("the recreate should have worked, got {outcome:?}");
        };
        assert_eq!(
            previous_sha, merge_request_sha,
            "where the branch was is reported"
        );
        assert_ne!(recreated_sha, merge_request_sha);
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "mr/dev^"]),
            base
        );
        assert_eq!(
            installed_git_output(&directory, &["diff", "--name-only", &base, "mr/dev"]),
            "a.txt\nb.txt\nc.txt\nd.txt"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );

        // (e): a dirty tree and a detached HEAD are refused with nothing moved.
        std::fs::write(directory.join("a.txt"), "changed\n").unwrap();
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &[&shas["A"]], "mr/dirty", false),
            cx,
        )
        .await;
        assert!(
            matches!(&outcome, MergeRequestBuildOutcome::Refused { left_on_scratch_branch: false, reason } if reason.contains("uncommitted changes")),
            "{outcome:?}"
        );
        installed_git_output(&directory, &["checkout", "--", "a.txt"]);
        installed_git_output(&directory, &["checkout", "--detach"]);
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &[&shas["A"]], "mr/detached", false),
            cx,
        )
        .await;
        assert!(
            matches!(&outcome, MergeRequestBuildOutcome::Refused { left_on_scratch_branch: false, reason } if reason.contains("detached HEAD")),
            "{outcome:?}"
        );
        installed_git_output(&directory, &["checkout", "dev"]);
        for branch in ["mr/dirty", "mr/detached"] {
            assert!(
                !installed_git(&directory, &["rev-parse", "--verify", "--quiet", branch])
                    .status
                    .success(),
                "{branch} must not have been created"
            );
        }
        assert_eq!(scratch_branches(&directory), "");

        // (f): a conflict stops on the scratch branch; abandoning it goes back to `dev`.
        installed_git_output(&directory, &["checkout", "main"]);
        std::fs::write(directory.join("base.txt"), "main\n").unwrap();
        installed_git_output(&directory, &["commit", "-am", "main moves on"]);
        installed_git_output(&directory, &["checkout", "dev"]);
        std::fs::write(directory.join("base.txt"), "dev\n").unwrap();
        installed_git_output(&directory, &["commit", "-am", "dev changes base"]);
        let conflicting = installed_git_output(&directory, &["rev-parse", "HEAD"]);
        let build = merge_request_build("main", &[&shas["A"], &conflicting], "mr/conflict", false);
        let scratch = build.scratch_branch.clone();
        let outcome = run_merge_request_build(&repository, build.clone(), cx).await;
        let MergeRequestBuildOutcome::Stopped { base_sha, .. } = outcome else {
            panic!("the conflict should stop the build, got {outcome:?}");
        };
        assert_eq!(
            base_sha,
            installed_git_output(&directory, &["rev-parse", "main"])
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            scratch
        );
        repository
            .update(cx, |repository, cx| {
                repository.sequencer_abort(SequencerKind::CherryPick, cx)
            })
            .await
            .unwrap()
            .unwrap();
        let left = repository
            .update(cx, |repository, cx| {
                repository.leave_rewrite_scratch_branch("dev".to_string(), scratch.clone(), cx)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(left.succeeded(), "{}", left.failure_message());
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );
        assert_eq!(scratch_branches(&directory), "");
        assert_eq!(
            installed_git_output(&directory, &["status", "--porcelain"]),
            ""
        );

        // The same conflict, resolved and continued, then finished from the box.
        let outcome = run_merge_request_build(&repository, build, cx).await;
        let MergeRequestBuildOutcome::Stopped { base_sha, .. } = outcome else {
            panic!("the conflict should stop the build again, got {outcome:?}");
        };
        std::fs::write(directory.join("base.txt"), "resolved\n").unwrap();
        installed_git_output(&directory, &["add", "base.txt"]);
        let continued = repository
            .update(cx, |repository, cx| {
                repository.sequencer_continue(SequencerKind::CherryPick, cx)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(continued.succeeded(), "{}", continued.failure_message());
        let continuation = MergeRequestContinuation {
            base_sha: base_sha.clone(),
            merge_request_branch: "mr/conflict".to_string(),
            message: "MR にまとめる".to_string(),
            recreate: false,
            origin_branch: "dev".to_string(),
            scratch_branch: scratch.clone(),
        };

        // "続きの手順" pressed from another branch folds nothing there, even one that starts from
        // the base just like the scratch branch: `reset --soft` only ever runs on the scratch
        // branch.
        installed_git_output(&directory, &["checkout", "-b", "look-alike", &scratch]);
        let look_alike_before = installed_git_output(&directory, &["rev-parse", "look-alike"]);
        let elsewhere = repository
            .update(cx, |repository, cx| {
                repository.finish_merge_request_branch(continuation.clone(), cx)
            })
            .await
            .unwrap();
        assert!(elsewhere.is_err(), "{elsewhere:?}");
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "look-alike"]),
            look_alike_before
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "look-alike"
        );
        assert!(
            !installed_git(
                &directory,
                &["rev-parse", "--verify", "--quiet", "mr/conflict"]
            )
            .status
            .success()
        );
        installed_git_output(&directory, &["checkout", &scratch]);
        installed_git_output(&directory, &["branch", "-D", "look-alike"]);

        let finished = repository
            .update(cx, |repository, cx| {
                repository.finish_merge_request_branch(continuation, cx)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                finished,
                MergeRequestBuildOutcome::Created {
                    previous_sha: None,
                    return_failure: None,
                    ..
                }
            ),
            "{finished:?}"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "mr/conflict^"]),
            base_sha
        );
        assert_eq!(
            installed_git_output(&directory, &["show", "mr/conflict:base.txt"]),
            "resolved"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );
        assert_eq!(scratch_branches(&directory), "");

        // (g): the base itself is never the branch that gets recreated.
        let main_before = installed_git_output(&directory, &["rev-parse", "main"]);
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &[&shas["A"]], "main", true),
            cx,
        )
        .await;
        assert!(
            matches!(&outcome, MergeRequestBuildOutcome::Refused { left_on_scratch_branch: false, reason } if reason.contains("The base branch")),
            "{outcome:?}"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "main"]),
            main_before
        );

        // Nor the default branch, even with the base elsewhere and "作り直す" ticked.
        installed_git_output(
            &directory,
            &["update-ref", "refs/remotes/origin/main", "main"],
        );
        installed_git_output(
            &directory,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        for recreate in [false, true] {
            let outcome = run_merge_request_build(
                &repository,
                merge_request_build(&base, &[&shas["A"]], "main", recreate),
                cx,
            )
            .await;
            assert!(
                matches!(&outcome, MergeRequestBuildOutcome::Refused { left_on_scratch_branch: false, reason } if reason.contains("default branch")),
                "{outcome:?}"
            );
        }
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "main"]),
            main_before
        );

        // (h): a file the user's branch ignores but the base tracks is neither overwritten on the
        // way to the scratch branch nor deleted on the way back.
        installed_git_output(&directory, &["checkout", "main"]);
        std::fs::write(directory.join("local.cfg"), "base\n").unwrap();
        installed_git_output(&directory, &["add", "local.cfg"]);
        installed_git_output(&directory, &["commit", "-m", "track local.cfg"]);
        installed_git_output(&directory, &["checkout", "dev"]);
        std::fs::write(directory.join(".gitignore"), "local.cfg\n").unwrap();
        installed_git_output(&directory, &["add", ".gitignore"]);
        installed_git_output(&directory, &["commit", "-m", "ignore local.cfg"]);
        std::fs::write(directory.join("local.cfg"), "my secret\n").unwrap();
        let outcome = run_merge_request_build(
            &repository,
            merge_request_build("main", &[&shas["A"]], "mr/ignored", false),
            cx,
        )
        .await;
        assert!(
            matches!(
                &outcome,
                MergeRequestBuildOutcome::Refused {
                    left_on_scratch_branch: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("local.cfg")).unwrap(),
            "my secret\n",
            "the ignored file must survive"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );
        assert_eq!(scratch_branches(&directory), "");
    }

    /// The merge-request branch is pushed while the user stays on their own branch: a new one
    /// gets its upstream, a recreated one goes with a lease, and the lease holds when somebody
    /// else moved the branch on the remote in the meantime.
    #[gpui::test]
    async fn test_push_a_branch_that_is_not_checked_out_on_installed_git(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let remote = temp_dir.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        installed_git_output(&remote, &["init", "--bare"]);
        let directory = temp_dir.path().join("repo");
        let shas = merge_request_repository(&directory);
        let remote_url = remote.to_string_lossy().to_string();
        installed_git_output(&directory, &["remote", "add", "origin", &remote_url]);
        installed_git_output(&directory, &["push", "origin", "main"]);
        installed_git_output(&directory, &["branch", "mr/dev", &shas["B"]]);

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [directory.as_path()],
            cx,
        )
        .await;
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let repository = project.read_with(cx, |project, cx| {
            project
                .active_repository(cx)
                .expect("the temporary repository should be found")
        });
        let push = |options: Option<PushOptions>, cx: &mut TestAppContext| {
            let (askpass, _prompts) = test_askpass_delegate(cx);
            repository.update(cx, |repository, cx| {
                repository.push(
                    "mr/dev".into(),
                    "mr/dev".into(),
                    "origin".into(),
                    options,
                    askpass,
                    cx,
                )
            })
        };

        push(Some(PushOptions::SetUpstream), cx)
            .await
            .unwrap()
            .expect("a new branch should push");
        assert_eq!(
            installed_git_output(&remote, &["rev-parse", "mr/dev"]),
            shas["B"]
        );
        assert_eq!(
            installed_git_output(
                &directory,
                &["rev-parse", "--abbrev-ref", "mr/dev@{upstream}"]
            ),
            "origin/mr/dev"
        );
        assert_eq!(
            installed_git_output(&directory, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev",
            "pushing never switches branches"
        );

        installed_git_output(&directory, &["branch", "-f", "mr/dev", &shas["C"]]);
        push(Some(PushOptions::Force), cx)
            .await
            .unwrap()
            .expect("a recreated branch should push with a lease");
        assert_eq!(
            installed_git_output(&remote, &["rev-parse", "mr/dev"]),
            shas["C"]
        );

        // Somebody else pushes to the branch; the lease is still on what this repository last
        // saw, so the next forced push is refused instead of throwing their commit away.
        installed_git_output(&remote, &["update-ref", "refs/heads/mr/dev", &shas["A"]]);
        installed_git_output(&directory, &["branch", "-f", "mr/dev", &shas["B"]]);
        assert!(
            push(Some(PushOptions::Force), cx).await.unwrap().is_err(),
            "the lease should refuse to overwrite somebody else's push"
        );
        assert_eq!(
            installed_git_output(&remote, &["rev-parse", "mr/dev"]),
            shas["A"]
        );
    }
}

/// This snapshot computes the repository state on the foreground thread while
/// running the git commands on the background thread. We update branch, head,
/// remotes, and worktrees first so the UI can react sooner, then compute file
/// state and emit those events immediately after.
///
/// `scan` says which of those two halves to recompute; the other one is carried
/// over from the previous snapshot. A refs-only scan still escalates to the file
/// state pass when it turns out the checked-out commit moved, since that changes
/// what `git status` reports even though no worktree or index event was seen.
async fn compute_snapshot(
    this: Entity<Repository>,
    backend: Arc<dyn GitRepository>,
    scan: SnapshotScan,
    cx: &mut AsyncApp,
) -> RepositorySnapshot {
    log::debug!("starting compute snapshot: {scan:?}");

    let (id, work_directory_abs_path, prev_snapshot, untracked_files) =
        this.update(cx, |this, cx| {
            if scan.statuses {
                this.paths_needing_status_update.clear();
            }
            (
                this.id,
                this.work_directory_abs_path.clone(),
                this.snapshot.clone(),
                ProjectSettings::get_global(cx).git.status_untracked_files,
            )
        });

    let mut scan_statuses = scan.statuses;
    let snapshot = if scan.refs {
        let branches_future = {
            let backend = backend.clone();
            async move { backend.branches().await.log_err().unwrap_or_default() }
        };
        let head_commit_future = {
            let backend = backend.clone();
            async move { backend.show("HEAD".to_string()).await.ok() }
        };
        let worktrees_future = {
            let backend = backend.clone();
            async move { backend.worktrees().await.log_err().unwrap_or_default() }
        };
        let (branches, head_commit, all_worktrees) =
            futures::future::join3(branches_future, head_commit_future, worktrees_future).await;
        log::debug!("fetched branches, head commit, worktrees");

        let BranchesScanResult {
            branches,
            error: branch_list_error,
            ref_storage,
        } = branches;
        let branch = branches.iter().find(|branch| branch.is_head).cloned();
        let branch_list: Arc<[Branch]> = branches.into();

        let linked_worktrees: Arc<[GitWorktree]> = all_worktrees
            .into_iter()
            .filter(|wt| wt.path != *work_directory_abs_path)
            .collect();

        let mut remote_urls = backend.remote_urls().await;
        let remote_origin_url = remote_urls.remove("origin");
        let remote_upstream_url = remote_urls.remove("upstream");

        log::debug!("fetched remotes");

        // Only refs were reported as changed, but a ref update that moved the
        // checked-out commit (`git reset`, an amended commit) changes what
        // `git status` reports as well, so the status pass can only be skipped
        // when HEAD stayed put.
        if head_commit.as_ref().map(|commit| &commit.sha)
            != prev_snapshot.head_commit.as_ref().map(|commit| &commit.sha)
            || branch.as_ref().map(|branch| &branch.ref_name)
                != prev_snapshot.branch.as_ref().map(|branch| &branch.ref_name)
        {
            scan_statuses = true;
        }

        this.update(cx, |this, cx| {
            let head_changed =
                branch != this.snapshot.branch || head_commit != this.snapshot.head_commit;
            let branch_list_changed = *branch_list != *this.snapshot.branch_list;
            let branch_list_error_changed = branch_list_error != this.snapshot.branch_list_error;
            let worktrees_changed = *linked_worktrees != *this.snapshot.linked_worktrees;

            this.snapshot = RepositorySnapshot {
                id,
                work_directory_abs_path,
                branch,
                branch_list: branch_list.clone(),
                branch_list_error,
                head_commit,
                remote_origin_url,
                remote_upstream_url,
                linked_worktrees,
                scan_id: prev_snapshot.scan_id + 1,
                ..prev_snapshot
            };

            if head_changed {
                cx.emit(RepositoryEvent::HeadChanged);
            }

            if branch_list_changed || branch_list_error_changed {
                cx.emit(RepositoryEvent::BranchListChanged);
            }

            this.apply_ref_storage_scan(ref_storage, cx);

            if worktrees_changed {
                cx.emit(RepositoryEvent::GitWorktreeListChanged);
            }

            this.snapshot.clone()
        })
    } else {
        this.update(cx, |this, _| {
            this.snapshot = RepositorySnapshot {
                id,
                work_directory_abs_path,
                scan_id: prev_snapshot.scan_id + 1,
                ..prev_snapshot
            };
            this.snapshot.clone()
        })
    };

    if !scan_statuses {
        return snapshot;
    }

    let statuses_future = {
        let backend = backend.clone();
        async move {
            backend
                .status(
                    &[RepoPath::from_rel_path(
                        &RelPath::new(".".as_ref(), PathStyle::local()).unwrap(),
                    )],
                    untracked_files,
                )
                .await
        }
    };
    let diff_stats_future = {
        let snapshot = snapshot.clone();
        let backend = backend.clone();
        async move {
            if snapshot.head_commit.is_some() {
                futures::future::join3(
                    backend.diff_stat(DiffStatType::HeadToWorktree, &[]),
                    backend.diff_stat(DiffStatType::HeadToIndex, &[]),
                    backend.diff_stat(DiffStatType::IndexToWorktree, &[]),
                )
                .await
            } else {
                (
                    Ok(status::GitDiffStat::default()),
                    Ok(status::GitDiffStat::default()),
                    Ok(status::GitDiffStat::default()),
                )
            }
        }
    };
    let stash_entries_future = {
        let backend = backend.clone();
        async move { backend.stash_entries().await.log_err().unwrap_or_default() }
    };

    let (statuses, diff_stats, stash_entries) =
        futures::future::join3(statuses_future, diff_stats_future, stash_entries_future).await;
    let (statuses, status_error) = match statuses {
        Ok(statuses) => (statuses, None),
        Err(error) => {
            log::error!(
                "failed to read git statuses in {:?}: {error:#}",
                snapshot.work_directory_abs_path
            );
            (
                status::GitStatus::default(),
                Some(SharedString::from(format!("{error:#}"))),
            )
        }
    };
    let (diff_stats, staged_diff_stats, unstaged_diff_stats) = diff_stats;
    let diff_stats = diff_stats.log_err().unwrap_or_default();
    let staged_diff_stats = staged_diff_stats.log_err().unwrap_or_default();
    let unstaged_diff_stats = unstaged_diff_stats.log_err().unwrap_or_default();
    log::debug!("fetched statuses, diff stats, stash entries");

    let diff_stat_map: HashMap<&RepoPath, DiffStat> =
        diff_stats.entries.iter().map(|(p, s)| (p, *s)).collect();
    let staged_diff_stat_map: HashMap<&RepoPath, DiffStat> = staged_diff_stats
        .entries
        .iter()
        .map(|(p, s)| (p, *s))
        .collect();
    let unstaged_diff_stat_map: HashMap<&RepoPath, DiffStat> = unstaged_diff_stats
        .entries
        .iter()
        .map(|(p, s)| (p, *s))
        .collect();
    let mut conflicted_paths = Vec::new();
    let statuses_by_path = SumTree::from_iter(
        statuses.entries.iter().map(|(repo_path, status)| {
            if status.is_conflicted() {
                conflicted_paths.push(repo_path.clone());
            }
            StatusEntry {
                repo_path: repo_path.clone(),
                status: *status,
                diff_stat: diff_stat_map.get(repo_path).copied(),
                staged_diff_stat: staged_diff_stat_map.get(repo_path).copied(),
                unstaged_diff_stat: unstaged_diff_stat_map.get(repo_path).copied(),
            }
        }),
        (),
    );

    let (merge_details, conflicts_changed) = cx
        .background_spawn({
            let backend = backend.clone();
            let mut merge_details = snapshot.merge.clone();
            async move {
                let conflicts_changed = merge_details.update(&backend, conflicted_paths).await;
                (merge_details, conflicts_changed)
            }
        })
        .await;
    log::debug!("new merge details: {merge_details:?}");

    this.update(cx, |this, cx| {
        if conflicts_changed
            || statuses_by_path != this.snapshot.statuses_by_path
            || status_error != this.snapshot.status_error
        {
            cx.emit(RepositoryEvent::StatusesChanged);
        }
        if stash_entries != this.snapshot.stash_entries {
            cx.emit(RepositoryEvent::StashEntriesChanged);
        }

        this.snapshot.scan_id += 1;
        this.snapshot.merge = merge_details;
        this.snapshot.statuses_by_path = statuses_by_path;
        this.snapshot.stash_entries = stash_entries;
        this.snapshot.status_error = status_error;

        this.snapshot.clone()
    })
}

fn status_from_proto(
    simple_status: i32,
    status: Option<proto::GitFileStatus>,
) -> anyhow::Result<FileStatus> {
    use proto::git_file_status::Variant;

    let Some(variant) = status.and_then(|status| status.variant) else {
        let code = proto::GitStatus::try_from(simple_status)
            .ok()
            .with_context(|| format!("Invalid git status code: {simple_status}"))?;
        let result = match code {
            proto::GitStatus::Added => TrackedStatus {
                worktree_status: StatusCode::Added,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            proto::GitStatus::Modified => TrackedStatus {
                worktree_status: StatusCode::Modified,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            proto::GitStatus::Conflict => UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::Updated,
            }
            .into(),
            proto::GitStatus::Deleted => TrackedStatus {
                worktree_status: StatusCode::Deleted,
                index_status: StatusCode::Unmodified,
            }
            .into(),
            _ => anyhow::bail!("Invalid code for simple status: {simple_status}"),
        };
        return Ok(result);
    };

    let result = match variant {
        Variant::Untracked(_) => FileStatus::Untracked,
        Variant::Ignored(_) => FileStatus::Ignored,
        Variant::Unmerged(unmerged) => {
            let [first_head, second_head] =
                [unmerged.first_head, unmerged.second_head].map(|head| {
                    let code = proto::GitStatus::try_from(head)
                        .ok()
                        .with_context(|| format!("Invalid git status code: {head}"))?;
                    let result = match code {
                        proto::GitStatus::Added => UnmergedStatusCode::Added,
                        proto::GitStatus::Updated => UnmergedStatusCode::Updated,
                        proto::GitStatus::Deleted => UnmergedStatusCode::Deleted,
                        _ => anyhow::bail!("Invalid code for unmerged status: {code:?}"),
                    };
                    Ok(result)
                });
            let [first_head, second_head] = [first_head?, second_head?];
            UnmergedStatus {
                first_head,
                second_head,
            }
            .into()
        }
        Variant::Tracked(tracked) => {
            let [index_status, worktree_status] = [tracked.index_status, tracked.worktree_status]
                .map(|status| {
                    let code = proto::GitStatus::try_from(status)
                        .ok()
                        .with_context(|| format!("Invalid git status code: {status}"))?;
                    let result = match code {
                        proto::GitStatus::Modified => StatusCode::Modified,
                        proto::GitStatus::TypeChanged => StatusCode::TypeChanged,
                        proto::GitStatus::Added => StatusCode::Added,
                        proto::GitStatus::Deleted => StatusCode::Deleted,
                        proto::GitStatus::Renamed => StatusCode::Renamed,
                        proto::GitStatus::Copied => StatusCode::Copied,
                        proto::GitStatus::Unmodified => StatusCode::Unmodified,
                        _ => anyhow::bail!("Invalid code for tracked status: {code:?}"),
                    };
                    Ok(result)
                });
            let [index_status, worktree_status] = [index_status?, worktree_status?];
            TrackedStatus {
                index_status,
                worktree_status,
            }
            .into()
        }
    };
    Ok(result)
}

fn status_to_proto(status: FileStatus) -> proto::GitFileStatus {
    use proto::git_file_status::{Tracked, Unmerged, Variant};

    let variant = match status {
        FileStatus::Untracked => Variant::Untracked(Default::default()),
        FileStatus::Ignored => Variant::Ignored(Default::default()),
        FileStatus::Unmerged(UnmergedStatus {
            first_head,
            second_head,
        }) => Variant::Unmerged(Unmerged {
            first_head: unmerged_status_to_proto(first_head),
            second_head: unmerged_status_to_proto(second_head),
        }),
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status,
        }) => Variant::Tracked(Tracked {
            index_status: tracked_status_to_proto(index_status),
            worktree_status: tracked_status_to_proto(worktree_status),
        }),
    };
    proto::GitFileStatus {
        variant: Some(variant),
    }
}

fn unmerged_status_to_proto(code: UnmergedStatusCode) -> i32 {
    match code {
        UnmergedStatusCode::Added => proto::GitStatus::Added as _,
        UnmergedStatusCode::Deleted => proto::GitStatus::Deleted as _,
        UnmergedStatusCode::Updated => proto::GitStatus::Updated as _,
    }
}

fn tracked_status_to_proto(code: StatusCode) -> i32 {
    match code {
        StatusCode::Added => proto::GitStatus::Added as _,
        StatusCode::Deleted => proto::GitStatus::Deleted as _,
        StatusCode::Modified => proto::GitStatus::Modified as _,
        StatusCode::Renamed => proto::GitStatus::Renamed as _,
        StatusCode::TypeChanged => proto::GitStatus::TypeChanged as _,
        StatusCode::Copied => proto::GitStatus::Copied as _,
        StatusCode::Unmodified => proto::GitStatus::Unmodified as _,
    }
}
