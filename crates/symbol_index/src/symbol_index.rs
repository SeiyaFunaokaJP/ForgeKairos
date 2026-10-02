//! A name-based symbol index for C and C++ that works without compilation information.
//!
//! The index is built from tree-sitter parses of each file and records definitions, call sites
//! (with the function containing them) and function pointer wiring (`.open = foo_open`). Names
//! are matched textually, so results are candidates rather than the exact answers a language
//! server gives; callers are expected to prefer LSP results and label these as name matches.
//!
//! The engine does not touch the project model: the caller lists the files and supplies their
//! contents through [`FileSource`], and is responsible for exclusions such as private files.

mod cleanup;
mod extract;
mod storage;
mod type_layout;
mod type_resolve;

use std::{
    fs::{self, File},
    io, ops,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow};
use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest as _, Sha256};

use crate::{
    extract::{ExtractedFile, LanguageGuess, MAX_QUALIFIED_NAMES, language_for_path},
    storage::{
        ContentHash, MANIFEST_FILE_NAME, Manifest, ManifestSegment, MetadataOverride, Segment,
        SegmentFileInput, create_private_directory, is_valid_relative_path,
        parse_segment_file_name, private_file_options, write_atomically,
    },
};

pub use crate::cleanup::*;
pub use crate::extract::{
    AssignmentKind, CallKind, DefinitionKind, Position, Range, SourceLanguage,
};
pub use crate::type_layout::*;
pub use crate::type_resolve::*;

/// Modification times closer than this to the moment a file was read cannot prove that the
/// file did not change again within the same timestamp tick, so such files are re-hashed.
const RACY_WINDOW_NANOS: u64 = 2_000_000_000;
/// Names per unit of work when a search scans all names.
const SEARCH_RUN_NAMES: u32 = 16 * 1024;
/// Compaction starts once this many small or mostly superseded segments have accumulated.
const COMPACTION_TRIGGER: usize = 4;
/// Segments are cut by source size as well as file count, so a segment with few files can
/// still be large; only segments below this size count as small.
const SMALL_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
const LOCK_FILE_NAME: &str = "lock";
/// The manifest is rewritten at most this often while files keep changing (a branch switch
/// changes thousands over several seconds), at the end of every full update, and by
/// [`SymbolIndex::save`]. What was committed since the last write is redone by the full update
/// that follows a crash.
pub const MANIFEST_WRITE_INTERVAL: Duration = Duration::from_secs(5);
/// Rewritten each time the index is opened; its modification time is when the index was
/// last used (see [`index_last_used`]).
const LAST_USED_FILE_NAME: &str = "last-used";

#[derive(Clone, Debug)]
pub struct IndexConfig {
    /// Larger files (generated tables, amalgamations) are skipped and counted.
    pub max_file_bytes: u64,
    /// Worker threads for parsing; `None` uses the available parallelism.
    pub parallelism: Option<usize>,
    /// A parse that takes longer is abandoned and the file is recorded as failed.
    pub parse_timeout: Duration,
    /// Files per segment written during an update.
    pub segment_file_limit: usize,
    /// Source bytes per segment written during an update, bounding memory while indexing.
    pub segment_source_byte_limit: u64,
    /// Verify the checksum of every segment when opening. Costs a read of the whole index.
    pub verify_checksums: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 4 * 1024 * 1024,
            parallelism: None,
            parse_timeout: Duration::from_secs(10),
            segment_file_limit: 4096,
            segment_source_byte_limit: 96 * 1024 * 1024,
            verify_checksums: true,
        }
    }
}

/// A file to index. `path` is relative to the index root and uses `/` separators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: Arc<str>,
    pub mtime: Option<SystemTime>,
    pub size: u64,
}

/// Supplies file contents by index-relative path.
pub trait FileSource: Sync {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
}

/// Reads files from a directory on the local file system.
pub struct DirectorySource {
    root: PathBuf,
}

impl DirectorySource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl FileSource for DirectorySource {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        if !is_valid_relative_path(path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path:?} is not a path inside the index root"),
            ));
        }
        let mut full_path = self.root.clone();
        for component in path.split('/') {
            full_path.push(component);
        }
        fs::read(full_path)
    }
}

/// Whether the engine would index a file with this path (by extension).
pub fn is_indexable_path(path: &str) -> bool {
    language_for_path(path).is_some()
}

/// Lists the indexable files under `root`, honoring `.gitignore` and hidden-file rules. This is
/// for tests, benchmarks and tools; the editor lists files from its own worktree model.
pub fn scan_directory(root: &Path) -> Result<Vec<FileEntry>> {
    let entries = Mutex::new(Vec::new());
    let errors = AtomicUsize::new(0);
    ignore::WalkBuilder::new(root)
        .follow_links(false)
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        log::debug!("symbol_index: skipping an entry while scanning: {error}");
                        errors.fetch_add(1, Ordering::Relaxed);
                        return ignore::WalkState::Continue;
                    }
                };
                if !entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_file())
                {
                    return ignore::WalkState::Continue;
                }
                let Ok(relative) = entry.path().strip_prefix(root) else {
                    return ignore::WalkState::Continue;
                };
                let mut path = String::new();
                for component in relative.components() {
                    let Some(component) = component.as_os_str().to_str() else {
                        return ignore::WalkState::Continue;
                    };
                    if !path.is_empty() {
                        path.push('/');
                    }
                    path.push_str(component);
                }
                if !is_indexable_path(&path) {
                    return ignore::WalkState::Continue;
                }
                match entry.metadata() {
                    Ok(metadata) => entries.lock().push(FileEntry {
                        path: path.into(),
                        mtime: metadata.modified().ok(),
                        size: metadata.len(),
                    }),
                    Err(error) => {
                        log::debug!("symbol_index: cannot stat {path}: {error}");
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                ignore::WalkState::Continue
            })
        });
    let mut entries = entries.into_inner();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let errors = errors.into_inner();
    if errors > 0 {
        log::info!("symbol_index: {errors} entries under {root:?} could not be read");
    }
    Ok(entries)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressPhase {
    Indexing,
    Compacting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub phase: ProgressPhase,
    pub completed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Files passed in that the engine can index.
    pub files_considered: usize,
    pub files_unchanged: usize,
    pub files_parsed: usize,
    /// Timestamp changed but the contents hash did not, so the file was not parsed again.
    pub files_touched: usize,
    pub files_removed: usize,
    pub files_skipped_too_large: usize,
    pub files_skipped_unsupported: usize,
    pub files_unreadable: usize,
    pub files_parse_failed: usize,
    pub files_with_syntax_errors: usize,
    pub segments_written: usize,
    pub segments_compacted: usize,
    pub cancelled: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files: usize,
    pub definitions: u64,
    pub calls: u64,
    pub assignments: u64,
    pub files_with_syntax_errors: usize,
    pub files_parse_failed: usize,
    pub segments: usize,
    pub disk_bytes: u64,
    /// From the most recent full update.
    pub skipped_too_large: u64,
    /// From the most recent full update.
    pub skipped_unsupported: u64,
}

/// Identifies a definition in the current state of the index. Ids stay valid until an update
/// rewrites the file (or compaction moves it), after which lookups by id return `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefinitionId {
    segment: u64,
    index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    pub id: DefinitionId,
    pub name: String,
    pub kind: DefinitionKind,
    pub is_static: bool,
    /// A prototype, a method declared in a class body, or an `extern` variable.
    pub is_declaration: bool,
    pub path: Arc<str>,
    pub language: SourceLanguage,
    /// The declared name.
    pub name_range: Range,
    /// The whole definition, including a function's body.
    pub item_range: Range,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub path: Arc<str>,
    /// The called name; for member calls, the field name (`open` in `ops->open(...)`).
    pub callee: String,
    pub kind: CallKind,
    /// Found in the body of a `#define` (then `caller` is the macro).
    pub in_macro_body: bool,
    /// The range of the called name.
    pub range: Range,
    /// The function (or macro) containing the call; `None` at file scope.
    pub caller: Option<Definition>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallersResult {
    pub calls: Vec<CallSite>,
    /// More call sites exist than the limit allowed.
    pub truncated: bool,
}

/// What a called name refers to, decided when querying because a call to a function-like macro
/// is syntactically a function call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalleeTarget {
    Function,
    Macro,
    /// The name is defined both as a function and as a macro (often under different `#if`s).
    FunctionOrMacro,
    /// A call through a struct member; see [`SymbolIndex::assignments`] for candidates.
    Indirect,
    /// No definition with this name is indexed (a library function, or generated code).
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalleeSite {
    pub path: Arc<str>,
    pub callee: String,
    pub kind: CallKind,
    pub in_macro_body: bool,
    pub range: Range,
    pub target: CalleeTarget,
}

/// A function stored into a struct member: `.open = foo_open` or `ops->open = foo_open`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotAssignment {
    pub field: String,
    pub function: String,
    /// The struct type of the initialized variable, when the declaration names it.
    pub container: Option<String>,
    pub kind: AssignmentKind,
    pub path: Arc<str>,
    /// The range of the assigned function name.
    pub range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// Case-insensitive prefix.
    Prefix,
    /// Case-insensitive substring.
    Substring,
    /// Case-insensitive subsequence, ranked by how compact the match is.
    Fuzzy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolMatch {
    pub definition: Definition,
    pub score: u32,
}

/// Everything recorded for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSymbols {
    pub path: Arc<str>,
    pub language: SourceLanguage,
    pub has_syntax_errors: bool,
    pub parse_failed: bool,
    pub definitions: Vec<Definition>,
    pub calls: Vec<CallSite>,
    pub assignments: Vec<SlotAssignment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileLocation {
    segment: u64,
    file_index: u32,
    mtime: u64,
    size: u64,
    verified_at: u64,
    hash: ContentHash,
}

impl FileLocation {
    /// A timestamp this close to the last verification does not prove the file is unchanged.
    fn is_racy(&self) -> bool {
        self.mtime == 0 || self.mtime.saturating_add(RACY_WINDOW_NANOS) >= self.verified_at
    }
}

#[derive(Clone)]
struct SegmentView {
    segment: Arc<Segment>,
    dead: Arc<Vec<u64>>,
    has_dead: bool,
}

impl SegmentView {
    fn is_live(&self, file_index: u32) -> bool {
        let word = (file_index / 64) as usize;
        let bit = file_index % 64;
        file_index < self.segment.file_count()
            && self
                .dead
                .get(word)
                .is_none_or(|&word| word & (1u64 << bit) == 0)
    }
}

#[derive(Default)]
struct Snapshot {
    segments: Vec<SegmentView>,
    segment_positions: FxHashMap<u64, usize>,
    files: FxHashMap<Arc<str>, FileLocation>,
    stats: IndexStats,
}

impl Snapshot {
    fn view(&self, segment: u64) -> Option<&SegmentView> {
        self.segments.get(*self.segment_positions.get(&segment)?)
    }
}

struct WriterState {
    /// What queries see. It is ahead of the manifest on disk until the next write.
    manifest: Manifest,
    /// Superseded segment files that could not be deleted yet (still mapped on Windows, or
    /// still named by the manifest on disk).
    pending_deletions: Vec<u64>,
    /// The segments named by the manifest on disk: their files must outlive that manifest.
    written_segments: FxHashSet<u64>,
    manifest_unwritten: bool,
    manifest_written_at: Instant,
    #[cfg(test)]
    manifest_writes: usize,
}

impl WriterState {
    /// Writes the manifest if it changed since it was last written. Until then a crash leaves
    /// the previous manifest, whose segments are all still on disk (see
    /// [`SymbolIndex::delete_pending`]); segments written since are unreferenced and removed
    /// when the index is opened again.
    fn write_manifest(&mut self, directory: &Path) -> Result<()> {
        if !self.manifest_unwritten {
            return Ok(());
        }
        write_atomically(&directory.join(MANIFEST_FILE_NAME), &self.manifest.encode())?;
        self.written_segments = self
            .manifest
            .segments
            .iter()
            .map(|segment| segment.id)
            .collect();
        self.manifest_unwritten = false;
        self.manifest_written_at = Instant::now();
        #[cfg(test)]
        {
            self.manifest_writes += 1;
        }
        Ok(())
    }

    fn new(manifest: Manifest) -> Self {
        Self {
            written_segments: manifest.segments.iter().map(|segment| segment.id).collect(),
            manifest,
            pending_deletions: Vec::new(),
            manifest_unwritten: false,
            manifest_written_at: Instant::now(),
            #[cfg(test)]
            manifest_writes: 0,
        }
    }
}

/// An open index. Queries take `&self` and run against a consistent snapshot while an update
/// is in progress; updates are serialized.
pub struct SymbolIndex {
    directory: PathBuf,
    config: IndexConfig,
    snapshot: RwLock<Arc<Snapshot>>,
    writer: Mutex<WriterState>,
    thread_pool: OnceLock<rayon::ThreadPool>,
    reset_reason: Option<String>,
    // Held for the lifetime of the index so that two processes never write the same directory.
    _lock_file: File,
}

fn system_time_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn now_nanos() -> u64 {
    system_time_nanos(SystemTime::now())
}

fn content_hash(contents: &[u8]) -> ContentHash {
    let digest = Sha256::digest(contents);
    let mut hash = [0u8; 16];
    if let Some(prefix) = digest.get(..16) {
        hash.copy_from_slice(prefix);
    }
    hash
}

enum Outcome {
    Parsed(Box<SegmentFileInput>, Option<FileLocation>),
    Touched(FileLocation, u64, u64),
    Vanished(Arc<str>),
    Unreadable,
    Cancelled,
}

struct Candidate<'a> {
    entry: &'a FileEntry,
    language: LanguageGuess,
    previous: Option<FileLocation>,
}

impl SymbolIndex {
    /// Opens the index in `index_dir`, creating it if needed. An index that cannot be read (a
    /// different format version, a damaged file) is discarded and starts empty; see
    /// [`SymbolIndex::reset_reason`].
    pub fn open(index_dir: &Path, config: IndexConfig) -> Result<Self> {
        create_private_directory(index_dir)?;
        let lock_file = private_file_options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(index_dir.join(LOCK_FILE_NAME))
            .with_context(|| format!("opening the lock file in {}", index_dir.display()))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(anyhow!(
                    "the symbol index in {} is in use by another process",
                    index_dir.display()
                ));
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| {
                    format!("locking the symbol index in {}", index_dir.display())
                });
            }
        }
        // Without the marker the index looks as old as its last change, and may be removed as
        // unused while it is used every day without changes.
        let marked = private_file_options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(index_dir.join(LAST_USED_FILE_NAME))
            .and_then(|mut file| io::Write::write_all(&mut file, &now_nanos().to_le_bytes()));
        if let Err(error) = marked {
            log::warn!(
                "symbol_index: cannot mark the index in {} as used: {error}",
                index_dir.display()
            );
        }

        let (mut manifest, segments, reset_reason) = match load(index_dir, config.verify_checksums)
        {
            Ok((manifest, segments)) => (manifest, segments, None),
            Err(error) => {
                let reason = format!("{error:#}");
                log::warn!(
                    "symbol_index: discarding the index in {}: {reason}",
                    index_dir.display()
                );
                // Continuing after the discarded segments' ids keeps a `DefinitionId` handed out
                // before the reset from naming a definition in the rebuilt index.
                let next_segment_id = highest_segment_id(index_dir)?
                    .and_then(|id| id.checked_add(1))
                    .filter(|&id| id <= u64::MAX / 2)
                    .unwrap_or(0);
                remove_index_files(index_dir, &FxHashSet::default())?;
                let manifest = Manifest {
                    next_segment_id,
                    ..Manifest::default()
                };
                write_atomically(&index_dir.join(MANIFEST_FILE_NAME), &manifest.encode())?;
                (manifest, Vec::new(), Some(reason))
            }
        };
        // Segments written after the manifest was last saved are removed below, but a client of
        // a remote server may still hold definition ids naming them, so their ids stay used.
        if let Some(next_segment_id) = highest_segment_id(index_dir)?
            .and_then(|id| id.checked_add(1))
            .filter(|&id| id <= u64::MAX / 2)
            && next_segment_id > manifest.next_segment_id
        {
            manifest.next_segment_id = next_segment_id;
        }
        let referenced: FxHashSet<u64> =
            manifest.segments.iter().map(|segment| segment.id).collect();
        remove_index_files(index_dir, &referenced)?;

        let snapshot = build_snapshot(index_dir, &manifest, &segments);
        Ok(Self {
            directory: index_dir.to_path_buf(),
            config,
            snapshot: RwLock::new(Arc::new(snapshot)),
            writer: Mutex::new(WriterState::new(manifest)),
            thread_pool: OnceLock::new(),
            reset_reason,
            _lock_file: lock_file,
        })
    }

    /// Why the index was discarded when it was opened, if it was.
    pub fn reset_reason(&self) -> Option<&str> {
        self.reset_reason.as_deref()
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn current(&self) -> Arc<Snapshot> {
        self.snapshot.read().clone()
    }

    fn thread_pool(&self) -> Result<&rayon::ThreadPool> {
        if let Some(pool) = self.thread_pool.get() {
            return Ok(pool);
        }
        let threads = self
            .config
            .parallelism
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |count| count.get()));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|index| format!("symbol-index-{index}"))
            .build()
            .context("starting the symbol index worker threads")?;
        Ok(self.thread_pool.get_or_init(|| pool))
    }

    /// Brings the index in line with `files`, the complete list of files under the root:
    /// new and modified files are parsed, missing ones are removed. Work is committed in
    /// segments, so a cancelled update keeps what it finished.
    pub fn update(
        &self,
        files: &[FileEntry],
        source: &dyn FileSource,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &AtomicBool,
    ) -> Result<UpdateStats> {
        let started = Instant::now();
        let mut writer = self.writer.lock();
        let snapshot = self.current();
        let mut stats = UpdateStats::default();
        let mut listed: FxHashSet<&str> = FxHashSet::default();
        let mut candidates = Vec::new();
        let mut removals: Vec<Arc<str>> = Vec::new();
        for entry in files {
            let Some(language) =
                language_for_path(&entry.path).filter(|_| is_valid_relative_path(&entry.path))
            else {
                stats.files_skipped_unsupported += 1;
                continue;
            };
            if entry.size > self.config.max_file_bytes {
                stats.files_skipped_too_large += 1;
                continue;
            }
            if !listed.insert(entry.path.as_ref()) {
                continue;
            }
            stats.files_considered += 1;
            let previous = snapshot.files.get(&entry.path).copied();
            let unchanged = previous.is_some_and(|previous| {
                previous.size == entry.size
                    && Some(previous.mtime) == entry.mtime.map(system_time_nanos)
                    && !previous.is_racy()
            });
            if unchanged {
                stats.files_unchanged += 1;
            } else {
                candidates.push(Candidate {
                    entry,
                    language,
                    previous,
                });
            }
        }
        for path in snapshot.files.keys() {
            if !listed.contains(path.as_ref()) {
                removals.push(path.clone());
            }
        }
        writer.manifest.skipped_too_large = stats.files_skipped_too_large as u64;
        writer.manifest.skipped_unsupported = stats.files_skipped_unsupported as u64;
        self.apply(
            &mut writer,
            candidates,
            removals,
            source,
            progress,
            cancel,
            &mut stats,
        )?;
        let published = self.current();
        if published.stats.skipped_too_large != writer.manifest.skipped_too_large
            || published.stats.skipped_unsupported != writer.manifest.skipped_unsupported
        {
            let manifest = writer.manifest.clone();
            self.commit(&mut writer, manifest, Vec::new())?;
        }
        self.write_manifest(&mut writer)?;
        self.delete_pending(&mut writer);
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// Applies file system events: `changed` files are re-read (and parsed if their contents
    /// changed), `removed` paths are dropped.
    pub fn update_files(
        &self,
        changed: &[FileEntry],
        removed: &[&str],
        source: &dyn FileSource,
        cancel: &AtomicBool,
    ) -> Result<UpdateStats> {
        let started = Instant::now();
        let mut writer = self.writer.lock();
        let snapshot = self.current();
        let mut stats = UpdateStats::default();
        let mut candidates = Vec::new();
        let mut removals: Vec<Arc<str>> = Vec::new();
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        for entry in changed {
            if !seen.insert(entry.path.as_ref()) {
                continue;
            }
            let previous = snapshot.files.get(&entry.path).copied();
            let Some(language) =
                language_for_path(&entry.path).filter(|_| is_valid_relative_path(&entry.path))
            else {
                stats.files_skipped_unsupported += 1;
                continue;
            };
            if entry.size > self.config.max_file_bytes {
                stats.files_skipped_too_large += 1;
                if previous.is_some() {
                    removals.push(entry.path.clone());
                }
                continue;
            }
            stats.files_considered += 1;
            candidates.push(Candidate {
                entry,
                language,
                previous,
            });
        }
        for &path in removed {
            if let Some((path, _)) = snapshot.files.get_key_value(path)
                && !seen.contains(path.as_ref())
            {
                removals.push(path.clone());
            }
        }
        self.apply(
            &mut writer,
            candidates,
            removals,
            source,
            &|_| {},
            cancel,
            &mut stats,
        )?;
        self.write_manifest_if_due(&mut writer)?;
        self.delete_pending(&mut writer);
        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        writer: &mut WriterState,
        mut candidates: Vec<Candidate>,
        removals: Vec<Arc<str>>,
        source: &dyn FileSource,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &AtomicBool,
        stats: &mut UpdateStats,
    ) -> Result<()> {
        if !removals.is_empty() {
            let snapshot = self.current();
            let mut manifest = writer.manifest.clone();
            for path in &removals {
                if let Some(location) = snapshot.files.get(path) {
                    tombstone(&mut manifest, location.segment, location.file_index);
                    stats.files_removed += 1;
                }
            }
            self.commit(writer, manifest, Vec::new())?;
        }

        candidates.sort_by(|left, right| left.entry.path.cmp(&right.entry.path));
        let total = candidates.len();
        let completed = AtomicUsize::new(0);
        let pool = self.thread_pool()?;
        let mut remaining = candidates.as_slice();
        while !remaining.is_empty() {
            if cancel.load(Ordering::Relaxed) {
                stats.cancelled = true;
                break;
            }
            let mut batch_length = 0;
            let mut batch_bytes = 0u64;
            for candidate in remaining {
                if batch_length > 0
                    && (batch_length >= self.config.segment_file_limit
                        || batch_bytes.saturating_add(candidate.entry.size)
                            > self.config.segment_source_byte_limit)
                {
                    break;
                }
                batch_length += 1;
                batch_bytes = batch_bytes.saturating_add(candidate.entry.size);
            }
            let (batch, rest) = remaining.split_at(batch_length);
            remaining = rest;

            let outcomes: Vec<Outcome> = pool.install(|| {
                batch
                    .par_iter()
                    .map(|candidate| {
                        let outcome = self.process(candidate, source, cancel);
                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        progress(Progress {
                            phase: ProgressPhase::Indexing,
                            completed: done,
                            total,
                        });
                        outcome
                    })
                    .collect()
            });
            // A batch that finished before the cancellation was noticed is still committed.
            if outcomes
                .iter()
                .any(|outcome| matches!(outcome, Outcome::Cancelled))
            {
                stats.cancelled = true;
                break;
            }

            let mut manifest = writer.manifest.clone();
            let mut parsed = Vec::new();
            for outcome in outcomes {
                match outcome {
                    Outcome::Parsed(input, previous) => {
                        if let Some(previous) = previous {
                            tombstone(&mut manifest, previous.segment, previous.file_index);
                        }
                        if input.extract.parse_failed {
                            stats.files_parse_failed += 1;
                        } else if input.extract.has_syntax_errors {
                            stats.files_with_syntax_errors += 1;
                        }
                        stats.files_parsed += 1;
                        parsed.push(*input);
                    }
                    Outcome::Touched(location, mtime, verified_at) => {
                        set_override(
                            &mut manifest,
                            MetadataOverride {
                                segment: location.segment,
                                file_index: location.file_index,
                                mtime,
                                verified_at,
                            },
                        );
                        stats.files_touched += 1;
                    }
                    Outcome::Vanished(path) => {
                        if let Some(location) = self.current().files.get(&path) {
                            tombstone(&mut manifest, location.segment, location.file_index);
                            stats.files_removed += 1;
                        }
                    }
                    Outcome::Unreadable => stats.files_unreadable += 1,
                    Outcome::Cancelled => {}
                }
            }
            let mut new_segments = Vec::new();
            if !parsed.is_empty() {
                new_segments.push(self.write_segment(&mut manifest, &parsed)?);
                stats.segments_written += 1;
            }
            self.commit(writer, manifest, new_segments)?;
        }

        if !stats.cancelled {
            self.compact(writer, progress, cancel, stats)?;
        }
        Ok(())
    }

    fn process(
        &self,
        candidate: &Candidate,
        source: &dyn FileSource,
        cancel: &AtomicBool,
    ) -> Outcome {
        if cancel.load(Ordering::Relaxed) {
            return Outcome::Cancelled;
        }
        let entry = candidate.entry;
        let verified_at = now_nanos();
        let contents = match source.read(&entry.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Outcome::Vanished(entry.path.clone());
            }
            Err(error) => {
                log::debug!("symbol_index: cannot read {}: {error}", entry.path);
                return Outcome::Unreadable;
            }
        };
        let hash = content_hash(&contents);
        let mtime = entry.mtime.map_or(0, system_time_nanos);
        if let Some(previous) = candidate.previous
            && previous.hash == hash
            && previous.size == contents.len() as u64
        {
            return Outcome::Touched(previous, mtime, verified_at);
        }
        let language = candidate.language.resolve(&contents);
        let extract = if contents.len() as u64 > self.config.max_file_bytes
            || u32::try_from(contents.len()).is_err()
        {
            // The file grew past the limit after it was listed.
            ExtractedFile {
                parse_failed: true,
                ..ExtractedFile::default()
            }
        } else {
            let parse_started = Instant::now();
            let timeout = self.config.parse_timeout;
            let extract = extract::extract(&contents, language, &|| {
                cancel.load(Ordering::Relaxed) || parse_started.elapsed() > timeout
            });
            if extract.parse_failed && cancel.load(Ordering::Relaxed) {
                return Outcome::Cancelled;
            }
            if extract.parse_failed {
                log::info!(
                    "symbol_index: gave up parsing {} after {:?}",
                    entry.path,
                    parse_started.elapsed()
                );
            }
            extract
        };
        Outcome::Parsed(
            Box::new(SegmentFileInput {
                path: entry.path.clone(),
                mtime,
                size: entry.size,
                verified_at,
                hash,
                language,
                extract,
            }),
            candidate.previous,
        )
    }

    fn write_segment(
        &self,
        manifest: &mut Manifest,
        files: &[SegmentFileInput],
    ) -> Result<Arc<Segment>> {
        let id = manifest.next_segment_id;
        manifest.next_segment_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow!("the symbol index ran out of segment ids"))?;
        storage::write_segment(&Segment::path(&self.directory, id), now_nanos(), files)?;
        let segment = Segment::open(&self.directory, id, false)?;
        manifest.segments.push(ManifestSegment {
            id,
            file_count: segment.file_count(),
            dead: vec![0; (segment.file_count() as usize).div_ceil(64)],
        });
        Ok(Arc::new(segment))
    }

    fn commit(
        &self,
        writer: &mut WriterState,
        mut manifest: Manifest,
        new_segments: Vec<Arc<Segment>>,
    ) -> Result<()> {
        let previous = self.current();
        let mut segments: FxHashMap<u64, Arc<Segment>> = previous
            .segments
            .iter()
            .map(|view| (view.segment.id, view.segment.clone()))
            .collect();
        for segment in new_segments {
            segments.insert(segment.id, segment);
        }
        // A fully superseded segment holds nothing; drop it from the manifest right away.
        let mut emptied = Vec::new();
        manifest.segments.retain(|entry| {
            let live = (0..entry.file_count).any(|index| {
                entry
                    .dead
                    .get((index / 64) as usize)
                    .is_none_or(|&word| word & (1u64 << (index % 64)) == 0)
            });
            if !live {
                emptied.push(entry.id);
            }
            live
        });
        manifest
            .overrides
            .retain(|metadata| !emptied.contains(&metadata.segment));

        let ordered: Vec<Arc<Segment>> = manifest
            .segments
            .iter()
            .filter_map(|entry| segments.get(&entry.id).cloned())
            .collect();
        let snapshot = derive_snapshot(
            &self.directory,
            &previous,
            &writer.manifest,
            &manifest,
            &ordered,
        );
        #[cfg(test)]
        {
            let rebuilt = build_snapshot(&self.directory, &manifest, &ordered);
            assert_eq!(snapshot.files, rebuilt.files, "derived file table");
            assert_eq!(snapshot.stats, rebuilt.stats, "derived statistics");
            assert_eq!(snapshot.segment_positions, rebuilt.segment_positions);
        }
        let removed: Vec<u64> = writer
            .manifest
            .segments
            .iter()
            .map(|entry| entry.id)
            .filter(|id| !manifest.segments.iter().any(|entry| entry.id == *id))
            .collect();
        writer.pending_deletions.extend(removed);
        writer.manifest = manifest;
        writer.manifest_unwritten = true;
        *self.snapshot.write() = Arc::new(snapshot);
        self.write_manifest_if_due(writer)
    }

    fn write_manifest_if_due(&self, writer: &mut WriterState) -> Result<()> {
        if writer.manifest_written_at.elapsed() >= MANIFEST_WRITE_INTERVAL {
            writer.write_manifest(&self.directory)?;
        }
        Ok(())
    }

    fn write_manifest(&self, writer: &mut WriterState) -> Result<()> {
        writer.write_manifest(&self.directory)
    }

    /// Writes what updates committed without writing it yet (they write at most every
    /// [`MANIFEST_WRITE_INTERVAL`]), and removes the segment files no longer needed. Meant for
    /// when updates have stopped; it does nothing when everything is written.
    pub fn save(&self) -> Result<()> {
        let mut writer = self.writer.lock();
        self.write_manifest(&mut writer)?;
        self.delete_pending(&mut writer);
        Ok(())
    }

    fn delete_pending(&self, writer: &mut WriterState) {
        let live: FxHashSet<u64> = writer
            .manifest
            .segments
            .iter()
            .map(|segment| segment.id)
            .collect();
        writer.pending_deletions.retain(|&id| {
            if live.contains(&id) {
                log::error!("symbol_index: segment {id} is live and pending deletion");
                return false;
            }
            // The manifest on disk still names it; deleting it now would make a crash discard
            // the whole index.
            if writer.written_segments.contains(&id) {
                return true;
            }
            let path = Segment::path(&self.directory, id);
            match fs::remove_file(&path) {
                Ok(()) => false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    log::debug!(
                        "symbol_index: will retry deleting {}: {error}",
                        path.display()
                    );
                    true
                }
            }
        });
    }

    /// Rewrites small or mostly superseded segments into full ones, so the number of segments
    /// a query visits stays bounded and superseded records stop taking disk space.
    fn compact(
        &self,
        writer: &mut WriterState,
        progress: &(dyn Fn(Progress) + Sync),
        cancel: &AtomicBool,
        stats: &mut UpdateStats,
    ) -> Result<()> {
        let snapshot = self.current();
        let small_limit = (self.config.segment_file_limit / 4).max(1) as u32;
        let mut selected: Vec<&SegmentView> = Vec::new();
        let mut large_and_mostly_dead = false;
        for view in &snapshot.segments {
            let file_count = view.segment.file_count();
            let live = (0..file_count).filter(|&index| view.is_live(index)).count() as u32;
            let mostly_dead = live * 2 < file_count;
            let small =
                file_count < small_limit && view.segment.byte_length() < SMALL_SEGMENT_BYTES;
            if mostly_dead || small {
                selected.push(view);
                large_and_mostly_dead |= mostly_dead && !small;
            }
        }
        if selected.len() < COMPACTION_TRIGGER && !large_and_mostly_dead {
            return Ok(());
        }

        let mut live_files: Vec<(&SegmentView, u32)> = Vec::new();
        for view in &selected {
            for index in 0..view.segment.file_count() {
                if view.is_live(index) {
                    live_files.push((view, index));
                }
            }
        }
        let total = live_files.len();
        let mut manifest = writer.manifest.clone();
        let selected_ids: Vec<u64> = selected.iter().map(|view| view.segment.id).collect();
        manifest
            .segments
            .retain(|entry| !selected_ids.contains(&entry.id));
        manifest
            .overrides
            .retain(|metadata| !selected_ids.contains(&metadata.segment));
        let mut new_segments = Vec::new();
        let mut done = 0;
        for chunk in live_files.chunks(self.config.segment_file_limit.max(1)) {
            if cancel.load(Ordering::Relaxed) {
                // Nothing has been committed, so the segments written so far are unreferenced.
                // Their ids must still not be handed out again: a file that could not be
                // deleted yet would otherwise be replaced by a live segment and then deleted.
                writer
                    .pending_deletions
                    .extend(new_segments.iter().map(|segment: &Arc<Segment>| segment.id));
                writer.manifest.next_segment_id = manifest.next_segment_id;
                return Ok(());
            }
            let mut inputs = Vec::with_capacity(chunk.len());
            for &(view, index) in chunk {
                // An unreadable record is dropped; the next full update finds the file missing
                // from the index and parses it again.
                let Some((record, extract)) = view.segment.materialize(index) else {
                    log::warn!(
                        "symbol_index: dropping unreadable file {index} of segment {}",
                        view.segment.id
                    );
                    continue;
                };
                let Some(path) = view.segment.file_path(&record) else {
                    log::warn!(
                        "symbol_index: dropping file {index} of segment {} with an unreadable path",
                        view.segment.id
                    );
                    continue;
                };
                let path: Arc<str> = Arc::from(path);
                let location = snapshot.files.get(&path).copied();
                inputs.push(SegmentFileInput {
                    mtime: location.map_or(record.mtime, |location| location.mtime),
                    verified_at: location
                        .map_or(record.verified_at, |location| location.verified_at),
                    path,
                    size: record.size,
                    hash: record.hash,
                    language: record.language,
                    extract,
                });
            }
            done += chunk.len();
            progress(Progress {
                phase: ProgressPhase::Compacting,
                completed: done,
                total,
            });
            if !inputs.is_empty() {
                new_segments.push(self.write_segment(&mut manifest, &inputs)?);
            }
        }
        stats.segments_compacted += selected.len();
        self.commit(writer, manifest, new_segments)
    }

    pub fn stats(&self) -> IndexStats {
        self.current().stats.clone()
    }

    fn definition_at(&self, view: &SegmentView, index: u32) -> Option<Definition> {
        let record = view.segment.definition(index)?;
        if !view.is_live(record.file) {
            return None;
        }
        let file = view.segment.file(record.file)?;
        Some(Definition {
            id: DefinitionId {
                segment: view.segment.id,
                index,
            },
            name: view.segment.name_text(record.name)?.to_owned(),
            kind: record.kind,
            is_static: record.is_static,
            is_declaration: record.is_declaration,
            path: Arc::from(view.segment.file_path(&file)?),
            language: file.language,
            name_range: record.name_range,
            item_range: record.item_range,
        })
    }

    fn call_at(&self, view: &SegmentView, index: u32, with_caller: bool) -> Option<CallSite> {
        let record = view.segment.call(index)?;
        if !view.is_live(record.file) {
            return None;
        }
        let file = view.segment.file(record.file)?;
        let callee = view.segment.name_text(record.name)?.to_owned();
        Some(CallSite {
            path: Arc::from(view.segment.file_path(&file)?),
            range: Range::token(record.position, callee.len()),
            callee,
            kind: record.kind,
            in_macro_body: record.in_macro_body,
            caller: if with_caller {
                record
                    .caller
                    .and_then(|caller| self.definition_at(view, caller))
            } else {
                None
            },
        })
    }

    fn assignment_at(&self, view: &SegmentView, index: u32) -> Option<SlotAssignment> {
        let record = view.segment.assignment(index)?;
        if !view.is_live(record.file) {
            return None;
        }
        let file = view.segment.file(record.file)?;
        let function = view.segment.name_text(record.value)?.to_owned();
        Some(SlotAssignment {
            field: view.segment.name_text(record.field)?.to_owned(),
            range: Range::token(record.position, function.len()),
            function,
            container: match record.container {
                Some(container) => Some(view.segment.name_text(container)?.to_owned()),
                None => None,
            },
            kind: record.kind,
            path: Arc::from(view.segment.file_path(&file)?),
        })
    }

    fn definitions_in(&self, snapshot: &Snapshot, name: &str) -> Vec<Definition> {
        let mut definitions = Vec::new();
        for view in &snapshot.segments {
            let Some(name_record) = view
                .segment
                .find_name(name)
                .and_then(|name_index| view.segment.name(name_index))
            else {
                continue;
            };
            for definition_index in view.segment.definition_postings(name_record.definitions) {
                if let Some(definition) = self.definition_at(view, definition_index) {
                    definitions.push(definition);
                }
            }
        }
        definitions.sort_by(|left, right| {
            (left.is_declaration, &left.path, left.name_range.start).cmp(&(
                right.is_declaration,
                &right.path,
                right.name_range.start,
            ))
        });
        definitions
    }

    /// Definitions and declarations named exactly `name`; definitions come before
    /// declarations.
    pub fn definitions(&self, name: &str) -> Vec<Definition> {
        self.definitions_in(&self.current(), name)
    }

    pub fn definition(&self, id: DefinitionId) -> Option<Definition> {
        let snapshot = self.current();
        self.definition_at(snapshot.view(id.segment)?, id.index)
    }

    /// Call sites whose called name is `callee_name`, with the function containing each.
    pub fn callers(&self, callee_name: &str, limit: usize) -> CallersResult {
        let snapshot = self.current();
        let mut result = CallersResult::default();
        'segments: for view in &snapshot.segments {
            let Some(name_record) = view
                .segment
                .find_name(callee_name)
                .and_then(|name_index| view.segment.name(name_index))
            else {
                continue;
            };
            for call_index in view.segment.call_postings(name_record.calls) {
                let Some(call) = self.call_at(view, call_index, true) else {
                    continue;
                };
                if result.calls.len() >= limit {
                    result.truncated = true;
                    break 'segments;
                }
                result.calls.push(call);
            }
        }
        result.calls.sort_by(|left, right| {
            (&left.path, left.range.start).cmp(&(&right.path, right.range.start))
        });
        result
    }

    /// The calls made by a function (or, for a macro, found in its body). `None` when the id
    /// no longer refers to a live definition.
    pub fn callees(&self, definition: DefinitionId) -> Option<Vec<CalleeSite>> {
        let snapshot = self.current();
        let view = snapshot.view(definition.segment)?;
        let record = view.segment.definition(definition.index)?;
        if !view.is_live(record.file) {
            return None;
        }
        let mut targets: FxHashMap<String, CalleeTarget> = FxHashMap::default();
        let mut callees = Vec::new();
        for call_index in view.segment.call_run(record.calls) {
            let Some(call) = self.call_at(view, call_index, false) else {
                continue;
            };
            let target = match call.kind {
                CallKind::Member => CalleeTarget::Indirect,
                CallKind::Direct => *targets
                    .entry(call.callee.clone())
                    .or_insert_with(|| self.target_of(&snapshot, &call.callee)),
            };
            callees.push(CalleeSite {
                path: call.path,
                callee: call.callee,
                kind: call.kind,
                in_macro_body: call.in_macro_body,
                range: call.range,
                target,
            });
        }
        callees.sort_by_key(|callee| callee.range.start);
        Some(callees)
    }

    fn target_of(&self, snapshot: &Snapshot, name: &str) -> CalleeTarget {
        let mut function = false;
        let mut macro_definition = false;
        for view in &snapshot.segments {
            let Some(name_record) = view
                .segment
                .find_name(name)
                .and_then(|name_index| view.segment.name(name_index))
            else {
                continue;
            };
            for definition_index in view.segment.definition_postings(name_record.definitions) {
                let Some(record) = view.segment.definition(definition_index) else {
                    continue;
                };
                if !view.is_live(record.file) {
                    continue;
                }
                function |= record.kind.is_function_like();
                macro_definition |= record.kind.is_macro();
            }
        }
        match (function, macro_definition) {
            (true, true) => CalleeTarget::FunctionOrMacro,
            (true, false) => CalleeTarget::Function,
            (false, true) => CalleeTarget::Macro,
            (false, false) => CalleeTarget::Unresolved,
        }
    }

    /// Functions stored into members named `field_name`: candidates for what
    /// `ops->field_name(...)` calls.
    pub fn assignments(&self, field_name: &str) -> Vec<SlotAssignment> {
        self.assignments_by(field_name, |segment, run| {
            segment.field_postings(run.fields).collect()
        })
    }

    /// Members that `function_name` is stored into: how a function is reached indirectly.
    pub fn assignments_of_function(&self, function_name: &str) -> Vec<SlotAssignment> {
        self.assignments_by(function_name, |segment, run| {
            segment.value_postings(run.values).collect()
        })
    }

    fn assignments_by(
        &self,
        name: &str,
        postings: impl Fn(&Segment, &storage::NameRecord) -> Vec<u32>,
    ) -> Vec<SlotAssignment> {
        let snapshot = self.current();
        let mut assignments = Vec::new();
        for view in &snapshot.segments {
            let Some(name_record) = view
                .segment
                .find_name(name)
                .and_then(|name_index| view.segment.name(name_index))
            else {
                continue;
            };
            for index in postings(&view.segment, &name_record) {
                if let Some(assignment) = self.assignment_at(view, index) {
                    assignments.push(assignment);
                }
            }
        }
        assignments.sort_by(|left, right| {
            (&left.path, left.range.start).cmp(&(&right.path, right.range.start))
        });
        assignments
    }

    /// The innermost function (or macro) definition in `path` whose extent contains `byte`.
    pub fn enclosing_definition(&self, path: &str, byte: u32) -> Option<Definition> {
        let snapshot = self.current();
        let location = snapshot.files.get(path)?;
        let view = snapshot.view(location.segment)?;
        let file = view.segment.file(location.file_index)?;
        let mut best: Option<Definition> = None;
        for index in view.segment.definition_run(file.definitions) {
            let Some(record) = view.segment.definition(index) else {
                continue;
            };
            let callable = (record.kind.is_function_like() && !record.is_declaration)
                || record.kind == DefinitionKind::FunctionMacro;
            if !callable || record.item_range.start.byte > byte || record.item_range.end.byte < byte
            {
                continue;
            }
            let length = record
                .item_range
                .end
                .byte
                .saturating_sub(record.item_range.start.byte);
            let better = best.as_ref().is_none_or(|best| {
                length
                    < best
                        .item_range
                        .end
                        .byte
                        .saturating_sub(best.item_range.start.byte)
            });
            if better {
                best = self.definition_at(view, index);
            }
        }
        best
    }

    /// Everything recorded for `path`, in source order.
    pub fn file_symbols(&self, path: &str) -> Option<FileSymbols> {
        let snapshot = self.current();
        let location = snapshot.files.get(path)?;
        let view = snapshot.view(location.segment)?;
        let file = view.segment.file(location.file_index)?;
        let definitions = view
            .segment
            .definition_run(file.definitions)
            .filter_map(|index| self.definition_at(view, index))
            .collect();
        let mut calls: Vec<CallSite> = view
            .segment
            .call_run(file.calls)
            .filter_map(|index| self.call_at(view, index, true))
            .collect();
        calls.sort_by_key(|call| call.range.start);
        let assignments = view
            .segment
            .assignment_run(file.assignments)
            .filter_map(|index| self.assignment_at(view, index))
            .collect();
        Some(FileSymbols {
            path: Arc::from(view.segment.file_path(&file)?),
            language: file.language,
            has_syntax_errors: file.has_syntax_errors(),
            parse_failed: file.parse_failed(),
            definitions,
            calls,
            assignments,
        })
    }

    /// Paths of all indexed files, sorted.
    pub fn files(&self) -> Vec<Arc<str>> {
        let mut files: Vec<Arc<str>> = self.current().files.keys().cloned().collect();
        files.sort();
        files
    }

    /// Definitions whose names match `query`, best first.
    pub fn search(&self, query: &str, mode: SearchMode, limit: usize) -> Vec<SymbolMatch> {
        let snapshot = self.current();
        let folded_query: Vec<u8> = query
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .collect();
        let folded_query = folded_query.as_slice();
        // A C++ function is also recorded under up to `MAX_QUALIFIED_NAMES` qualified names, and
        // only a qualified query matches those, so such a query reads enough names to fill the
        // results after the names of one place are merged.
        let query_is_qualified = query.contains("::");
        let name_limit = if query_is_qualified {
            limit.saturating_mul(MAX_QUALIFIED_NAMES)
        } else {
            limit
        };
        // Substring and fuzzy matching scan every name, so spread runs of names over the
        // workers (runs rather than whole segments, which differ in size and are fewer than the
        // workers on a large machine); a prefix is a binary search plus a short run and is
        // cheaper to do in place.
        let per_segment: Vec<Vec<(&str, u32)>> = match (mode, search_pool()) {
            (SearchMode::Substring | SearchMode::Fuzzy, Some(pool)) => {
                let runs: Vec<(&SegmentView, ops::Range<u32>)> = snapshot
                    .segments
                    .iter()
                    .flat_map(|view| {
                        let count = view.segment.name_count();
                        (0..count)
                            .step_by(SEARCH_RUN_NAMES as usize)
                            .map(move |start| {
                                (
                                    view,
                                    start..start.saturating_add(SEARCH_RUN_NAMES).min(count),
                                )
                            })
                    })
                    .collect();
                pool.install(|| {
                    runs.par_iter()
                        .map(|(view, names)| {
                            segment_candidates(
                                view,
                                names.clone(),
                                mode,
                                query,
                                folded_query,
                                name_limit,
                            )
                        })
                        .collect()
                })
            }
            _ => snapshot
                .segments
                .iter()
                .map(|view| {
                    segment_candidates(
                        view,
                        0..view.segment.name_count(),
                        mode,
                        query,
                        folded_query,
                        name_limit,
                    )
                })
                .collect(),
        };
        let mut candidates: Vec<(&str, u32)> = per_segment.into_iter().flatten().collect();
        // Ranked by score, then by name. A name's score depends only on the name, so its copies
        // from several segments fall in one group of equal scores; sorting by score alone first
        // keeps the name comparisons to the groups that reach the limit.
        candidates.sort_unstable_by_key(|candidate| std::cmp::Reverse(candidate.1));
        let mut ranked: Vec<(&str, u32)> = Vec::new();
        let mut group_start = 0;
        while ranked.len() < name_limit
            && let Some(&(_, score)) = candidates.get(group_start)
        {
            let group_length = candidates
                .get(group_start..)
                .unwrap_or_default()
                .partition_point(|candidate| candidate.1 == score);
            let group_end = group_start + group_length.max(1);
            if let Some(group) = candidates.get_mut(group_start..group_end) {
                group.sort_unstable_by(|left, right| rank_order(left, right, query_is_qualified));
                for &(name, score) in group.iter() {
                    if ranked.last().is_none_or(|&(last, _)| last != name) {
                        ranked.push((name, score));
                    }
                }
            }
            group_start = group_end;
        }
        // Every candidate has a definition, so the first `limit` names fill the results (of a
        // qualified query, once merged by place). Their definitions are looked up in every
        // segment, which is worth spreading over the workers.
        ranked.truncate(name_limit);
        let definitions: Vec<Vec<Definition>> = match search_pool() {
            Some(pool) if ranked.len() > 1 => pool.install(|| {
                ranked
                    .par_iter()
                    .map(|(name, _)| self.definitions_in(&snapshot, name))
                    .collect()
            }),
            _ => ranked
                .iter()
                .map(|(name, _)| self.definitions_in(&snapshot, name))
                .collect(),
        };
        if query_is_qualified {
            return merge_qualified_names(
                ranked.iter().map(|&(_, score)| score).zip(definitions),
                limit,
            );
        }
        let mut matches = Vec::new();
        for (&(_, score), definitions) in ranked.iter().zip(definitions) {
            for definition in definitions {
                if matches.len() >= limit {
                    return matches;
                }
                matches.push(SymbolMatch { definition, score });
            }
        }
        matches
    }
}

/// One place is listed once, under the name closest to the query. Names come best first, and
/// for one place that is the closest: the query itself scores highest, then the best score, and
/// equal scores rank the shorter qualified name first (see [`rank_order`]). Places come in the
/// order of their best name; a declaration and a definition are different places.
fn merge_qualified_names(
    ranked: impl Iterator<Item = (u32, Vec<Definition>)>,
    limit: usize,
) -> Vec<SymbolMatch> {
    let mut matches: Vec<SymbolMatch> = Vec::new();
    let mut places: FxHashSet<(Arc<str>, Position, Position)> = FxHashSet::default();
    for (score, definitions) in ranked {
        for definition in definitions {
            if matches.len() >= limit {
                return matches;
            }
            let place = (
                definition.path.clone(),
                definition.name_range.start,
                definition.name_range.end,
            );
            if places.insert(place) {
                matches.push(SymbolMatch { definition, score });
            }
        }
    }
    matches
}

impl Drop for SymbolIndex {
    fn drop(&mut self) {
        if let Err(error) = self.writer.get_mut().write_manifest(&self.directory) {
            log::warn!(
                "symbol_index: cannot save the index in {}: {error:#}",
                self.directory.display()
            );
        }
    }
}

/// Workers for searches, shared by every index in the process. Searches do not use the
/// indexing workers: rayon runs a job handed in from outside only once its workers run out of
/// their own work, so a search would wait for a whole batch of files to be parsed.
fn search_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().map_or(4, |count| count.get());
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("symbol-search-{index}"))
            .build()
            .map_err(|error| log::error!("symbol_index: starting the search threads: {error}"))
            .ok()
    })
    .as_ref()
}

/// Names in one segment that match and have a live definition there, with their scores.
/// Substring and fuzzy matching look only at the `names` run; a prefix looks at the whole
/// segment.
fn segment_candidates<'a>(
    view: &'a SegmentView,
    names: ops::Range<u32>,
    mode: SearchMode,
    query: &str,
    folded_query: &[u8],
    limit: usize,
) -> Vec<(&'a str, u32)> {
    let segment = &view.segment;
    let mut found = Vec::new();
    // A C++ function is also recorded as `Class::method`; those names only answer a query
    // that is qualified itself, so that `method` does not list the same function twice.
    let query_is_qualified = query.contains("::");
    let mut consider = |name_index: u32| -> bool {
        let Some(record) = segment.name(name_index) else {
            return true;
        };
        let Some(name) = segment.string_bytes(record.text) else {
            return true;
        };
        // A prefix scan ends at the first name past the prefix, qualified or not.
        if mode == SearchMode::Prefix && !starts_with_folded(name, folded_query) {
            return false;
        }
        if !query_is_qualified && name.contains(&b':') {
            return true;
        }
        let score = match mode {
            SearchMode::Prefix => prefix_score(name, query.as_bytes()),
            // Many names are only called or assigned; skipping them before scoring is cheap.
            _ if record.definitions.1 == 0 => return true,
            SearchMode::Substring => substring_score(name, query.as_bytes(), folded_query),
            SearchMode::Fuzzy => fuzzy_score(name, folded_query),
        };
        // Reading definition records only when some file is superseded keeps a scan over all
        // names from touching (and paging in) the definition section.
        let has_live_definition = || {
            if view.has_dead {
                segment
                    .definition_postings(record.definitions)
                    .any(|definition| {
                        segment
                            .definition(definition)
                            .is_some_and(|definition| view.is_live(definition.file))
                    })
            } else {
                record.definitions.1 > 0
            }
        };
        if let Some(score) = score
            && has_live_definition()
            && let Ok(name) = std::str::from_utf8(name)
        {
            found.push((name, score));
            if found.len() >= limit.saturating_mul(2).max(64) {
                keep_best(&mut found, limit, query_is_qualified);
            }
        }
        true
    };
    match (mode, rarest_byte(folded_query)) {
        (SearchMode::Prefix, _) => {
            let mut name_index = segment.folded_lower_bound(folded_query);
            while name_index < segment.name_count() && consider(name_index) {
                name_index += 1;
            }
        }
        // Both modes only match names containing every query byte, so only names containing
        // the rarest one are scored. Those are found by scanning the packed name text, which
        // is a fraction of the size of the name records a scan over all names reads.
        (SearchMode::Substring | SearchMode::Fuzzy, Some(anchor)) => {
            let text_end = names
                .end
                .checked_sub(1)
                .and_then(|last| segment.name(last))
                .map_or(0, |record| {
                    (record.text.0 as usize).saturating_add(record.text.1 as usize)
                });
            let text = segment.names_text();
            let text = text.get(..text_end.min(text.len())).unwrap_or_default();
            let anchor_offset = folded_query
                .iter()
                .position(|&byte| byte == anchor)
                .unwrap_or(0);
            let mut position = segment
                .name(names.start)
                .map_or(text.len(), |record| record.text.0 as usize);
            let mut cursor = names.start;
            while let Some(offset) = text
                .get(position..)
                .and_then(|rest| find_folded_byte(rest, anchor))
            {
                let hit = position + offset;
                // A substring must also surround the anchor; checking that in the packed text
                // first skips looking up the name for most hits.
                let match_start = match mode {
                    SearchMode::Substring => {
                        let surrounded = hit.checked_sub(anchor_offset).filter(|&start| {
                            start
                                .checked_add(folded_query.len())
                                .and_then(|end| text.get(start..end))
                                .is_some_and(|window| window.eq_ignore_ascii_case(folded_query))
                        });
                        let Some(start) = surrounded else {
                            position = hit + 1;
                            continue;
                        };
                        start
                    }
                    _ => hit,
                };
                let name_index = segment.name_at_text_offset(cursor, match_start);
                if name_index >= names.end {
                    break;
                }
                let Some(record) = segment.name(name_index) else {
                    break;
                };
                let start = record.text.0 as usize;
                let end = start.saturating_add(record.text.1 as usize);
                let match_end = match mode {
                    SearchMode::Substring => match_start.saturating_add(folded_query.len()),
                    _ => match_start + 1,
                };
                if start <= match_start && match_end <= end {
                    consider(name_index);
                    position = end;
                    cursor = name_index.saturating_add(1);
                } else {
                    // A substring match across two names, or a damaged segment; stepping one
                    // byte still ends the scan.
                    position = hit + 1;
                    cursor = name_index;
                }
            }
        }
        (SearchMode::Substring | SearchMode::Fuzzy, None) => {
            for name_index in names {
                consider(name_index);
            }
        }
    }
    keep_best(&mut found, limit, query_is_qualified);
    found
}

/// Keeps the best `limit` of `found`. Every name has a definition, so the best `limit` names
/// overall are among the best `limit` of each run of names; keeping only those keeps short
/// queries that match most names from building (and freeing) huge candidate lists.
fn keep_best(found: &mut Vec<(&str, u32)>, limit: usize, shorter_first: bool) {
    if limit == 0 {
        found.clear();
    } else if found.len() > limit {
        found.select_nth_unstable_by(limit - 1, |left, right| {
            rank_order(left, right, shorter_first)
        });
        found.truncate(limit);
    }
}

/// Best score first, then by name. Among the qualified names of one place the shorter one is
/// closer to the query, so with `shorter_first` (for qualified queries) equal scores rank the
/// shorter name first and the first name seen for a place is its closest one.
fn rank_order(left: &(&str, u32), right: &(&str, u32), shorter_first: bool) -> std::cmp::Ordering {
    let by_length = if shorter_first {
        left.0.len().cmp(&right.0.len())
    } else {
        std::cmp::Ordering::Equal
    };
    right
        .1
        .cmp(&left.1)
        .then(by_length)
        .then_with(|| left.0.cmp(right.0))
}

/// The query byte that the fewest names are expected to contain, by how often bytes occur in C
/// identifiers.
fn rarest_byte(folded_query: &[u8]) -> Option<u8> {
    const MOST_COMMON_FIRST: &[u8] = b"_etarisnoclduphmgfbkvwyx0123456789zjq";
    folded_query.iter().copied().max_by_key(|&byte| {
        MOST_COMMON_FIRST
            .iter()
            .position(|&common| common == byte)
            .unwrap_or(MOST_COMMON_FIRST.len())
    })
}

/// The first position of `folded_byte` in `haystack`, in either letter case.
fn find_folded_byte(haystack: &[u8], folded_byte: u8) -> Option<usize> {
    let upper = folded_byte.to_ascii_uppercase();
    if upper == folded_byte {
        memchr::memchr(folded_byte, haystack)
    } else {
        memchr::memchr2(folded_byte, upper, haystack)
    }
}

fn starts_with_folded(name: &[u8], folded_prefix: &[u8]) -> bool {
    name.len() >= folded_prefix.len()
        && name
            .iter()
            .zip(folded_prefix)
            .all(|(byte, prefix)| byte.to_ascii_lowercase() == *prefix)
}

fn length_penalty(name: &[u8]) -> u32 {
    u32::try_from(name.len()).unwrap_or(u32::MAX).min(200)
}

fn prefix_score(name: &[u8], query: &[u8]) -> Option<u32> {
    let mut score = 1000 - length_penalty(name);
    if name == query {
        score += 3000;
    } else if name.len() == query.len() {
        score += 2000;
    } else if name.starts_with(query) {
        score += 500;
    }
    Some(score)
}

fn substring_score(name: &[u8], query: &[u8], folded_query: &[u8]) -> Option<u32> {
    let position = if folded_query.is_empty() {
        0
    } else {
        name.windows(folded_query.len()).position(|window| {
            window
                .iter()
                .zip(folded_query)
                .all(|(byte, query_byte)| byte.to_ascii_lowercase() == *query_byte)
        })?
    };
    let mut score = 1000 - length_penalty(name);
    if position == 0 {
        score += 1000;
    } else if position
        .checked_sub(1)
        .and_then(|previous| name.get(previous))
        .is_some_and(|&previous| previous == b'_')
    {
        score += 500;
    }
    if !query.is_empty() && name.windows(query.len()).any(|window| window == query) {
        score += 200;
    }
    if name.len() == query.len() {
        score += 2000;
    }
    Some(score)
}

/// Every query character must appear in order. Matches at word starts (after `_`, or a
/// lowercase-to-uppercase change) and runs of consecutive matches score higher; gaps cost.
fn fuzzy_score(name: &[u8], folded_query: &[u8]) -> Option<u32> {
    let mut score: i64 = 1000 - i64::from(length_penalty(name));
    let mut search_from = 0usize;
    let mut previous_match: Option<usize> = None;
    for &query_byte in folded_query {
        let offset = name
            .get(search_from..)?
            .iter()
            .position(|byte| byte.to_ascii_lowercase() == query_byte)?;
        let position = search_from + offset;
        score += 10;
        let word_start = match position
            .checked_sub(1)
            .and_then(|previous| name.get(previous))
        {
            None => true,
            Some(&previous) => {
                previous == b'_'
                    || (previous.is_ascii_lowercase()
                        && name
                            .get(position)
                            .is_some_and(|current| current.is_ascii_uppercase()))
            }
        };
        if word_start {
            score += 30;
        }
        match previous_match {
            Some(previous) if previous + 1 == position => score += 20,
            Some(previous) => {
                score -= i64::try_from(position - previous - 1)
                    .unwrap_or(100)
                    .min(20)
            }
            None => score -= i64::try_from(position).unwrap_or(100).min(40),
        }
        previous_match = Some(position);
        search_from = position + 1;
    }
    if name.len() == folded_query.len() {
        score += 500;
    }
    Some(u32::try_from(score.max(1)).unwrap_or(1))
}

fn tombstone(manifest: &mut Manifest, segment: u64, file_index: u32) {
    if let Some(entry) = manifest
        .segments
        .iter_mut()
        .find(|entry| entry.id == segment)
        && let Some(word) = entry.dead.get_mut((file_index / 64) as usize)
    {
        *word |= 1u64 << (file_index % 64);
    }
    manifest
        .overrides
        .retain(|metadata| !(metadata.segment == segment && metadata.file_index == file_index));
}

fn set_override(manifest: &mut Manifest, metadata: MetadataOverride) {
    manifest.overrides.retain(|existing| {
        !(existing.segment == metadata.segment && existing.file_index == metadata.file_index)
    });
    manifest.overrides.push(metadata);
}

fn load(directory: &Path, verify_checksums: bool) -> Result<(Manifest, Vec<Arc<Segment>>)> {
    let manifest_path = directory.join(MANIFEST_FILE_NAME);
    let manifest = match fs::read(&manifest_path) {
        Ok(bytes) => Manifest::decode(&bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Manifest::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", manifest_path.display()));
        }
    };
    // Far beyond any id a real index reaches; such a value can only come from damage, and
    // would leave no ids for new segments.
    if manifest.next_segment_id > u64::MAX / 2 {
        return Err(anyhow!(
            "the manifest's next segment id {} is implausible",
            manifest.next_segment_id
        ));
    }
    let mut segments = Vec::with_capacity(manifest.segments.len());
    let mut seen = FxHashSet::default();
    for entry in &manifest.segments {
        if !seen.insert(entry.id) {
            return Err(anyhow!("segment {} is listed twice", entry.id));
        }
        let segment = Segment::open(directory, entry.id, verify_checksums)?;
        if segment.file_count() != entry.file_count {
            return Err(anyhow!(
                "segment {} has {} files, the manifest says {}",
                entry.id,
                segment.file_count(),
                entry.file_count
            ));
        }
        if entry.id >= manifest.next_segment_id {
            return Err(anyhow!("segment {} is newer than the manifest", entry.id));
        }
        segments.push(Arc::new(segment));
    }
    Ok((manifest, segments))
}

fn highest_segment_id(directory: &Path) -> Result<Option<u64>> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("listing index directory {}", directory.display()))?;
    let mut highest = None;
    for entry in entries {
        let entry = entry?;
        if let Some(id) = entry.file_name().to_str().and_then(parse_segment_file_name) {
            highest = highest.max(Some(id));
        }
    }
    Ok(highest)
}

/// Deletes index files in `directory` other than the manifest and the `keep` segments.
/// Only names this crate writes are touched.
fn remove_index_files(directory: &Path, keep: &FxHashSet<u64>) -> Result<()> {
    let entries = fs::read_dir(directory)
        .with_context(|| format!("listing index directory {}", directory.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let remove = if let Some(id) = parse_segment_file_name(name) {
            !keep.contains(&id)
        } else {
            name.ends_with(".bin.tmp") && (name.starts_with("seg-") || name.starts_with("manifest"))
        };
        if remove && let Err(error) = fs::remove_file(entry.path()) {
            log::debug!("symbol_index: cannot delete {name}: {error}");
        }
    }
    Ok(())
}

fn build_snapshot(directory: &Path, manifest: &Manifest, segments: &[Arc<Segment>]) -> Snapshot {
    derive_snapshot(
        directory,
        &Snapshot::default(),
        &Manifest::default(),
        manifest,
        segments,
    )
}

/// Builds the snapshot for `manifest` from `previous`, the snapshot of `previous_manifest`, by
/// reading only the file records that changed state: re-reading every file record for each
/// file system event takes tens of milliseconds on a tree with tens of thousands of files.
fn derive_snapshot(
    directory: &Path,
    previous: &Snapshot,
    previous_manifest: &Manifest,
    manifest: &Manifest,
    segments: &[Arc<Segment>],
) -> Snapshot {
    let mut snapshot = Snapshot::default();
    let by_id: FxHashMap<u64, &Arc<Segment>> = segments
        .iter()
        .map(|segment| (segment.id, segment))
        .collect();
    let overrides: FxHashMap<(u64, u32), &MetadataOverride> = manifest
        .overrides
        .iter()
        .map(|metadata| ((metadata.segment, metadata.file_index), metadata))
        .collect();
    let mut stats = IndexStats {
        skipped_too_large: manifest.skipped_too_large,
        skipped_unsupported: manifest.skipped_unsupported,
        disk_bytes: fs::metadata(directory.join(MANIFEST_FILE_NAME))
            .map_or(0, |metadata| metadata.len()),
        ..previous.stats.clone()
    };
    stats.segments = 0;
    for entry in &manifest.segments {
        let Some(&segment) = by_id.get(&entry.id) else {
            continue;
        };
        stats.segments += 1;
        stats.disk_bytes += segment.byte_length();
        snapshot
            .segment_positions
            .insert(entry.id, snapshot.segments.len());
        snapshot.segments.push(SegmentView {
            segment: segment.clone(),
            has_dead: entry.dead.iter().any(|&word| word != 0),
            dead: Arc::new(entry.dead.clone()),
        });
    }

    let mut files = previous.files.clone();
    files.retain(|_, location| {
        let live = snapshot
            .view(location.segment)
            .is_some_and(|view| view.is_live(location.file_index));
        if !live
            && let Some(record) = previous
                .view(location.segment)
                .and_then(|view| view.segment.file(location.file_index))
        {
            stats.remove_file(&record);
        }
        live
    });

    let file_location = |segment: u64, file_index: u32, record: &storage::FileRecord| {
        let metadata = overrides.get(&(segment, file_index));
        FileLocation {
            segment,
            file_index,
            mtime: metadata.map_or(record.mtime, |metadata| metadata.mtime),
            size: record.size,
            verified_at: metadata.map_or(record.verified_at, |metadata| metadata.verified_at),
            hash: record.hash,
        }
    };
    for view in &snapshot.segments {
        let segment = &view.segment;
        if previous.segment_positions.contains_key(&segment.id) {
            continue;
        }
        for file_index in 0..segment.file_count() {
            if !view.is_live(file_index) {
                continue;
            }
            let Some(record) = segment.file(file_index) else {
                continue;
            };
            let Some(path) = segment.file_path(&record) else {
                continue;
            };
            let location = file_location(segment.id, file_index, &record);
            if files.insert(Arc::from(path), location).is_some() {
                log::warn!("symbol_index: {path} is indexed twice");
            }
            stats.add_file(&record);
        }
    }

    // Files that stayed in place but whose recorded timestamps were set or dropped.
    let previous_overrides: FxHashSet<&MetadataOverride> =
        previous_manifest.overrides.iter().collect();
    let current_overrides: FxHashSet<&MetadataOverride> = manifest.overrides.iter().collect();
    for changed in previous_overrides.symmetric_difference(&current_overrides) {
        if !previous.segment_positions.contains_key(&changed.segment) {
            continue;
        }
        let Some(view) = snapshot.view(changed.segment) else {
            continue;
        };
        if !view.is_live(changed.file_index) {
            continue;
        }
        let Some(record) = view.segment.file(changed.file_index) else {
            continue;
        };
        let Some(path) = view.segment.file_path(&record) else {
            continue;
        };
        if let Some(location) = files.get_mut(path)
            && location.segment == changed.segment
            && location.file_index == changed.file_index
        {
            *location = file_location(changed.segment, changed.file_index, &record);
        }
    }

    stats.files = files.len();
    snapshot.files = files;
    snapshot.stats = stats;
    snapshot
}

impl IndexStats {
    fn add_file(&mut self, record: &storage::FileRecord) {
        self.definitions += u64::from(record.definitions.1);
        self.calls += u64::from(record.calls.1);
        self.assignments += u64::from(record.assignments.1);
        if record.parse_failed() {
            self.files_parse_failed += 1;
        } else if record.has_syntax_errors() {
            self.files_with_syntax_errors += 1;
        }
    }

    fn remove_file(&mut self, record: &storage::FileRecord) {
        self.definitions = self
            .definitions
            .saturating_sub(u64::from(record.definitions.1));
        self.calls = self.calls.saturating_sub(u64::from(record.calls.1));
        self.assignments = self
            .assignments
            .saturating_sub(u64::from(record.assignments.1));
        if record.parse_failed() {
            self.files_parse_failed = self.files_parse_failed.saturating_sub(1);
        } else if record.has_syntax_errors() {
            self.files_with_syntax_errors = self.files_with_syntax_errors.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod symbol_index_tests;
#[cfg(test)]
mod type_layout_tests;
