use notify::{
    Event, EventKind,
    event::{CreateKind, DataChange, ModifyKind, RemoveKind},
};
use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    ops::Bound,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

/// Background passes spread their metadata calls out so that polling a huge tree costs a
/// bounded amount of CPU and disk time; a pass over such a tree simply takes longer.
pub(super) const MAX_POLL_OPERATIONS_PER_SECOND: u64 = 10_000;
/// Shorter pauses are deferred and merged into a later, longer one.
const MIN_PACING_SLEEP: Duration = Duration::from_millis(20);
/// Paths that changed this recently are polled on every pass, ahead of the round-robin sweep
/// over the rest, since edits tend to come in bursts in the same few directories.
const HOT_WINDOW: Duration = Duration::from_secs(60);
/// File timestamps come from a coarse clock, so a write in the same tick as a snapshot can
/// leave both size and mtime unchanged. Like git's "racily clean" index entries, files whose
/// mtime is this close to their snapshot are reported once more on the next pass.
const RACY_WINDOW: Duration = Duration::from_secs(1);

pub(super) type FallbackEventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Polls each watched path without recursing into it, reporting what a single non-recursive
/// inotify watch on that path would report. Used for paths that must not hold an inotify watch.
pub(super) struct FallbackWatcher {
    entries: Mutex<FallbackEntries>,
    sink: FallbackEventSink,
}

#[derive(Default)]
struct FallbackEntries {
    by_path: BTreeMap<PathBuf, FallbackEntry>,
    next_generation: u64,
    /// Where the round-robin sweep over the paths that haven't changed lately resumes.
    sweep_cursor: Option<PathBuf>,
}

struct FallbackEntry {
    generation: u64,
    /// `None` while the path doesn't exist. It is then checked with a single stat per pass,
    /// and a new baseline is taken once it shows up again.
    snapshot: Option<Snapshot>,
    taken_at: SystemTime,
    last_changed: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Snapshot {
    Directory(BTreeMap<OsString, EntryState>),
    File(EntryState),
}

impl Snapshot {
    fn kind(&self) -> EntryKind {
        match self {
            Snapshot::Directory(_) => EntryKind::Directory,
            Snapshot::File(state) => state.kind,
        }
    }
}

struct ObservedSnapshot {
    snapshot: Snapshot,
    taken_at: SystemTime,
    operations: u64,
    /// False when some entries of a directory couldn't be listed.
    complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryState {
    kind: EntryKind,
    modified: Option<SystemTime>,
    len: u64,
    identity: Option<FileIdentity>,
}

/// Metadata that changes on writes which keep size and mtime (a rewrite followed by restoring
/// the mtime, or a rename over the file).
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    inode: u64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    mode: u32,
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt as _;
    FileIdentity {
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        mode: metadata.mode(),
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    attributes: u32,
    created: u64,
}

#[cfg(windows)]
fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::windows::fs::MetadataExt as _;
    FileIdentity {
        attributes: metadata.file_attributes(),
        created: metadata.creation_time(),
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity;

#[cfg(not(any(unix, windows)))]
fn file_identity(_metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity
}

impl EntryState {
    // A directory's own mtime changes whenever its contents do, which inotify does not report
    // to the parent's watch, so directories are compared by kind alone.
    const DIRECTORY: Self = Self {
        kind: EntryKind::Directory,
        modified: None,
        len: 0,
        identity: None,
    };

    fn from_metadata(file_type: fs::FileType, metadata: &fs::Metadata) -> Self {
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        if kind == EntryKind::Directory {
            return Self::DIRECTORY;
        }
        Self {
            kind,
            modified: metadata.modified().ok(),
            len: metadata.len(),
            identity: Some(file_identity(metadata)),
        }
    }

    fn is_racy(&self, taken_at: SystemTime) -> bool {
        self.kind != EntryKind::Directory
            && self.modified.is_some_and(|modified| {
                modified
                    .checked_add(RACY_WINDOW)
                    .is_some_and(|racy_until| racy_until > taken_at)
            })
    }
}

impl FallbackWatcher {
    pub(super) fn new(sink: FallbackEventSink) -> Self {
        Self {
            entries: Mutex::new(FallbackEntries::default()),
            sink,
        }
    }

    pub(super) fn watch(&self, path: &Path) -> notify::Result<()> {
        if self
            .entries
            .lock()
            .by_path
            .get(path)
            .is_some_and(|entry| entry.snapshot.is_some())
        {
            return Ok(());
        }

        // The baseline is taken now rather than on the next pass, so a change made right after
        // the registration is still reported.
        let observed = take_snapshot(path).map_err(|error| watch_error(error, path))?;
        let mut entries = self.entries.lock();
        let generation = entries.next_generation;
        entries.next_generation += 1;
        entries.by_path.insert(
            path.to_path_buf(),
            FallbackEntry {
                generation,
                snapshot: Some(observed.snapshot),
                taken_at: observed.taken_at,
                last_changed: None,
            },
        );
        Ok(())
    }

    pub(super) fn unwatch(&self, path: &Path) -> notify::Result<()> {
        self.entries.lock().by_path.remove(path);
        Ok(())
    }

    pub(super) fn has_vanished(&self, path: &Path) -> bool {
        self.entries
            .lock()
            .by_path
            .get(path)
            .is_some_and(|entry| entry.snapshot.is_none())
    }

    /// Polls the recently changed paths, then continues the sweep over the others until
    /// `operation_budget` filesystem operations are spent; `None` sweeps everything.
    /// `after_each_path` receives the operations spent on each path so the caller can pace.
    pub(super) fn poll_pass(
        &self,
        operation_budget: Option<u64>,
        after_each_path: &mut dyn FnMut(u64),
    ) {
        let now = Instant::now();
        let (hot, cold) = {
            let entries = self.entries.lock();
            let is_hot = |entry: &FallbackEntry| {
                entry
                    .last_changed
                    .is_some_and(|changed| now.saturating_duration_since(changed) < HOT_WINDOW)
            };
            // The sweep resumes after the path it stopped at and wraps around to it.
            let ordered: Box<dyn Iterator<Item = (&PathBuf, &FallbackEntry)> + '_> =
                match &entries.sweep_cursor {
                    Some(cursor) => Box::new(
                        entries
                            .by_path
                            .range::<Path, _>((Bound::Excluded(cursor.as_path()), Bound::Unbounded))
                            .chain(entries.by_path.range::<Path, _>((
                                Bound::Unbounded,
                                Bound::Included(cursor.as_path()),
                            ))),
                    ),
                    None => Box::new(entries.by_path.iter()),
                };
            let mut hot = Vec::new();
            let mut cold = Vec::new();
            for (path, entry) in ordered {
                let target = (path.clone(), entry.generation);
                if is_hot(entry) {
                    hot.push(target);
                } else {
                    cold.push(target);
                }
            }
            (hot, cold)
        };

        let mut spent = 0;
        for (path, generation) in hot {
            let operations = self.poll_entry(&path, Some(generation), now);
            spent += operations;
            after_each_path(operations);
        }

        let cold_count = cold.len();
        let mut swept = 0;
        let mut last_swept = None;
        for (path, generation) in cold {
            if operation_budget.is_some_and(|budget| spent >= budget) {
                break;
            }
            let operations = self.poll_entry(&path, Some(generation), now);
            spent += operations;
            after_each_path(operations);
            swept += 1;
            last_swept = Some(path);
        }
        self.entries.lock().sweep_cursor = if swept < cold_count { last_swept } else { None };
    }

    #[cfg(test)]
    pub(super) fn poll_once(&self) {
        self.poll_pass(None, &mut |_| {});
    }

    pub(super) fn poll_path(&self, path: &Path) {
        self.poll_entry(path, None, Instant::now());
    }

    fn poll_entry(&self, path: &Path, expected_generation: Option<u64>, now: Instant) -> u64 {
        let observed = take_snapshot(path);
        let operations = observed.as_ref().map_or(1, |observed| observed.operations);
        let mut events = Vec::new();
        {
            let mut entries = self.entries.lock();
            let Some(entry) = entries.by_path.get_mut(path) else {
                return operations;
            };
            // The path was unwatched and watched again while it was being read, so this
            // observation is older than the entry's baseline.
            if expected_generation.is_some_and(|generation| generation != entry.generation) {
                return operations;
            }
            match (entry.snapshot.take(), observed) {
                (Some(previous), Ok(mut observed)) => {
                    if !observed.complete
                        && let (
                            Snapshot::Directory(previous_children),
                            Snapshot::Directory(current_children),
                        ) = (&previous, &mut observed.snapshot)
                    {
                        carry_over_unlisted(previous_children, current_children);
                    }
                    if diff_snapshots(
                        path,
                        &previous,
                        entry.taken_at,
                        &observed.snapshot,
                        &mut events,
                    ) {
                        entry.snapshot = Some(observed.snapshot);
                        entry.taken_at = observed.taken_at;
                    }
                }
                (Some(previous), Err(error)) if path_is_gone(&error) => {
                    events.push(removed_event(path.to_path_buf(), previous.kind()));
                }
                (Some(previous), Err(error)) => {
                    log::debug!("failed to poll {path:?} for changes: {error}");
                    entry.snapshot = Some(previous);
                }
                (None, Ok(current)) => {
                    events.push(created_event(path.to_path_buf(), current.snapshot.kind()));
                    entry.snapshot = Some(current.snapshot);
                    entry.taken_at = current.taken_at;
                }
                (None, Err(_still_missing)) => {}
            }
            if !events.is_empty() {
                entry.last_changed = Some(now);
            }
        }

        for event in events {
            (self.sink)(event);
        }
        operations
    }
}

/// When a listing was incomplete, children it didn't return keep their previous state instead
/// of being reported as removed (and then created again on the next complete listing).
fn carry_over_unlisted(
    previous: &BTreeMap<OsString, EntryState>,
    current: &mut BTreeMap<OsString, EntryState>,
) {
    for (name, state) in previous {
        current.entry(name.clone()).or_insert(*state);
    }
}

/// Appends the events that turn `previous` into `current` and returns whether the watched path
/// still exists as the same kind of entry.
fn diff_snapshots(
    watched_path: &Path,
    previous: &Snapshot,
    previous_taken_at: SystemTime,
    current: &Snapshot,
    events: &mut Vec<Event>,
) -> bool {
    match (previous, current) {
        (Snapshot::Directory(previous_children), Snapshot::Directory(current_children)) => {
            for (name, previous_state) in previous_children {
                let child_path = watched_path.join(name);
                match current_children.get(name) {
                    None => events.push(removed_event(child_path, previous_state.kind)),
                    Some(current_state) if current_state.kind != previous_state.kind => {
                        events.push(removed_event(child_path.clone(), previous_state.kind));
                        events.push(created_event(child_path, current_state.kind));
                    }
                    Some(current_state) => {
                        if current_state != previous_state
                            || previous_state.is_racy(previous_taken_at)
                        {
                            events.push(modified_event(child_path));
                        }
                    }
                }
            }
            for (name, current_state) in current_children {
                if !previous_children.contains_key(name) {
                    events.push(created_event(watched_path.join(name), current_state.kind));
                }
            }
            true
        }
        (Snapshot::File(previous_state), Snapshot::File(current_state)) => {
            if current_state != previous_state || previous_state.is_racy(previous_taken_at) {
                events.push(modified_event(watched_path.to_path_buf()));
            }
            true
        }
        _ => {
            events.push(removed_event(watched_path.to_path_buf(), previous.kind()));
            false
        }
    }
}

fn take_snapshot(path: &Path) -> io::Result<ObservedSnapshot> {
    let taken_at = SystemTime::now();
    // Follows symlinks, as inotify does for the watched path itself.
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Ok(ObservedSnapshot {
            snapshot: Snapshot::File(EntryState::from_metadata(metadata.file_type(), &metadata)),
            taken_at,
            operations: 1,
            complete: true,
        });
    }

    let mut operations = 2;
    let mut complete = true;
    let mut children = BTreeMap::new();
    for child in fs::read_dir(path)? {
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                log::debug!("failed to list an entry of {path:?}: {error}");
                complete = false;
                continue;
            }
        };
        let file_type = match child.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                log::debug!("failed to read the type of {:?}: {error}", child.path());
                complete = false;
                continue;
            }
        };
        let state = if file_type.is_dir() {
            EntryState::DIRECTORY
        } else {
            operations += 1;
            match child.metadata() {
                Ok(metadata) => EntryState::from_metadata(file_type, &metadata),
                // The child disappeared while the directory was listed; leaving it out reports
                // it as removed.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    log::debug!("failed to read metadata of {:?}: {error}", child.path());
                    complete = false;
                    continue;
                }
            }
        };
        children.insert(child.file_name(), state);
    }
    Ok(ObservedSnapshot {
        snapshot: Snapshot::Directory(children),
        taken_at,
        operations,
        complete,
    })
}

fn path_is_gone(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn watch_error(error: io::Error, path: &Path) -> notify::Error {
    if error.kind() == io::ErrorKind::NotFound {
        notify::Error::path_not_found()
    } else {
        notify::Error::io(error)
    }
    .add_path(path.to_path_buf())
}

fn created_event(path: PathBuf, kind: EntryKind) -> Event {
    let create_kind = if kind == EntryKind::Directory {
        CreateKind::Folder
    } else {
        CreateKind::File
    };
    Event::new(EventKind::Create(create_kind)).add_path(path)
}

fn removed_event(path: PathBuf, kind: EntryKind) -> Event {
    let remove_kind = if kind == EntryKind::Directory {
        RemoveKind::Folder
    } else {
        RemoveKind::File
    };
    Event::new(EventKind::Remove(remove_kind)).add_path(path)
}

fn modified_event(path: PathBuf) -> Event {
    Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path)
}

/// Keeps a background pass at or below [`MAX_POLL_OPERATIONS_PER_SECOND`].
pub(super) struct PollPacer {
    started: Instant,
    operations: u64,
}

impl PollPacer {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            operations: 0,
        }
    }

    /// Records finished operations and returns how long to pause before continuing.
    pub(super) fn record(&mut self, operations: u64) -> Option<Duration> {
        self.operations = self.operations.saturating_add(operations);
        pacing_delay(
            self.operations,
            self.started.elapsed(),
            MAX_POLL_OPERATIONS_PER_SECOND,
        )
    }
}

fn pacing_delay(
    operations: u64,
    elapsed: Duration,
    operations_per_second: u64,
) -> Option<Duration> {
    let allotted =
        Duration::from_millis(operations.saturating_mul(1000) / operations_per_second.max(1));
    let delay = allotted.checked_sub(elapsed)?;
    (delay >= MIN_PACING_SLEEP).then_some(delay)
}

/// How many operations a background pass may spend on paths that haven't changed lately, so a
/// pass over a large tree ends in about one poll interval and changed paths are revisited soon.
pub(super) fn sweep_operation_budget(poll_interval: Duration) -> u64 {
    let milliseconds = u64::try_from(poll_interval.as_millis()).unwrap_or(u64::MAX);
    (MAX_POLL_OPERATIONS_PER_SECOND.saturating_mul(milliseconds) / 1000).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording_watcher() -> (FallbackWatcher, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink: FallbackEventSink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().push(event))
        };
        (FallbackWatcher::new(sink), events)
    }

    fn take_events(events: &Mutex<Vec<Event>>) -> Vec<(EventKind, PathBuf)> {
        std::mem::take(&mut *events.lock())
            .into_iter()
            .flat_map(|event| {
                let kind = event.kind;
                event.paths.into_iter().map(move |path| (kind, path))
            })
            .collect()
    }

    fn created_file(path: PathBuf) -> (EventKind, PathBuf) {
        (EventKind::Create(CreateKind::File), path)
    }

    fn created_folder(path: PathBuf) -> (EventKind, PathBuf) {
        (EventKind::Create(CreateKind::Folder), path)
    }

    fn removed_file(path: PathBuf) -> (EventKind, PathBuf) {
        (EventKind::Remove(RemoveKind::File), path)
    }

    fn removed_folder(path: PathBuf) -> (EventKind, PathBuf) {
        (EventKind::Remove(RemoveKind::Folder), path)
    }

    fn modified(path: PathBuf) -> (EventKind, PathBuf) {
        (EventKind::Modify(ModifyKind::Data(DataChange::Any)), path)
    }

    fn sorted(mut events: Vec<(EventKind, PathBuf)>) -> Vec<(EventKind, PathBuf)> {
        events.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| format!("{:?}", left.0).cmp(&format!("{:?}", right.0)))
        });
        events
    }

    /// Writes a file whose mtime lies well in the past, so it is not racily clean.
    fn write_settled_file(path: &Path, contents: &[u8]) -> SystemTime {
        fs::write(path, contents).expect("write file");
        let settled = SystemTime::now() - Duration::from_secs(60);
        set_modified(path, settled);
        settled
    }

    fn set_modified(path: &Path, modified: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open file")
            .set_modified(modified)
            .expect("set mtime");
    }

    #[test]
    fn reports_created_removed_and_modified_children() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");
        write_settled_file(&root.join("kept.txt"), b"a");
        write_settled_file(&root.join("removed.txt"), b"a");
        fs::create_dir(root.join("removed_dir")).expect("create removed dir");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::write(root.join("kept.txt"), b"abc").expect("modify kept file");
        fs::remove_file(root.join("removed.txt")).expect("remove file");
        fs::remove_dir(root.join("removed_dir")).expect("remove dir");
        write_settled_file(&root.join("new.txt"), b"a");
        fs::create_dir(root.join("new_dir")).expect("create dir");
        watcher.poll_once();

        assert_eq!(
            sorted(take_events(&events)),
            sorted(vec![
                modified(root.join("kept.txt")),
                removed_file(root.join("removed.txt")),
                removed_folder(root.join("removed_dir")),
                created_file(root.join("new.txt")),
                created_folder(root.join("new_dir")),
            ])
        );

        set_modified(
            &root.join("kept.txt"),
            SystemTime::now() - Duration::from_secs(60),
        );
        watcher.poll_once();
        take_events(&events);
        watcher.poll_once();
        assert_eq!(take_events(&events), Vec::new());
    }

    #[test]
    fn changes_inside_child_directories_are_not_reported_to_the_parent() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        let child = root.join("child");
        fs::create_dir_all(&child).expect("create child dir");
        fs::write(child.join("inner.txt"), b"a").expect("create inner file");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::write(child.join("inner.txt"), b"abc").expect("modify inner file");
        fs::write(child.join("added.txt"), b"a").expect("add inner file");
        fs::create_dir(child.join("grandchild")).expect("add grandchild dir");
        watcher.poll_once();

        assert_eq!(take_events(&events), Vec::new());
    }

    #[test]
    fn child_kind_changes_are_reported_as_remove_then_create() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");
        write_settled_file(&root.join("entry"), b"a");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::remove_file(root.join("entry")).expect("remove file entry");
        fs::create_dir(root.join("entry")).expect("create dir entry");
        watcher.poll_once();

        assert_eq!(
            take_events(&events),
            vec![
                removed_file(root.join("entry")),
                created_folder(root.join("entry")),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_keeping_size_and_mtime_is_reported() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");
        let file = root.join("same.txt");
        let settled = write_settled_file(&file, b"aaa");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::write(&file, b"bbb").expect("rewrite file");
        set_modified(&file, settled);
        watcher.poll_once();

        assert_eq!(take_events(&events), vec![modified(file)]);
    }

    #[test]
    fn racily_clean_files_are_reported_on_the_next_pass() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");
        let file = root.join("fresh.txt");
        fs::write(&file, b"aaa").expect("write file");
        let written = fs::metadata(&file)
            .and_then(|metadata| metadata.modified())
            .expect("read mtime");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        // A rewrite within the same timestamp tick leaves size and mtime as they were.
        fs::write(&file, b"bbb").expect("rewrite file");
        set_modified(&file, written);
        watcher.poll_once();

        assert_eq!(take_events(&events), vec![modified(file)]);
    }

    #[test]
    fn children_missing_from_an_incomplete_listing_keep_their_state() {
        let state = EntryState {
            kind: EntryKind::File,
            modified: None,
            len: 1,
            identity: None,
        };
        let previous = BTreeMap::from_iter([
            (OsString::from("listed"), state),
            (OsString::from("unlisted"), state),
        ]);
        let mut current =
            BTreeMap::from_iter([(OsString::from("listed"), EntryState { len: 2, ..state })]);

        carry_over_unlisted(&previous, &mut current);

        assert_eq!(current.get(&OsString::from("unlisted")), Some(&state));
        assert_eq!(
            current
                .get(&OsString::from("listed"))
                .map(|state| state.len),
            Some(2)
        );
    }

    #[test]
    fn vanished_watched_directory_is_reported_and_picked_up_again_when_it_returns() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");
        fs::write(root.join("file.txt"), b"a").expect("create file");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::remove_dir_all(&root).expect("remove watched dir");
        watcher.poll_once();
        assert_eq!(take_events(&events), vec![removed_folder(root.clone())]);
        assert!(watcher.has_vanished(&root));

        watcher.poll_once();
        assert_eq!(take_events(&events), Vec::new());

        fs::create_dir(&root).expect("recreate watched dir");
        watcher.poll_once();
        assert_eq!(take_events(&events), vec![created_folder(root.clone())]);
        assert!(!watcher.has_vanished(&root));

        write_settled_file(&root.join("after.txt"), b"a");
        watcher.poll_once();
        assert_eq!(
            take_events(&events),
            vec![created_file(root.join("after.txt"))]
        );

        watcher.unwatch(&root).expect("unwatch path");
    }

    #[test]
    fn watched_directory_replaced_by_a_file_counts_as_vanished() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");

        fs::remove_dir(&root).expect("remove watched dir");
        fs::write(&root, b"a").expect("create file in its place");
        watcher.poll_once();

        assert_eq!(take_events(&events), vec![removed_folder(root.clone())]);
        assert!(watcher.has_vanished(&root));
    }

    #[test]
    fn watched_file_reports_its_own_changes_and_removal() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let file = temp_dir.path().join("settings.json");
        write_settled_file(&file, b"{}");

        let (watcher, events) = recording_watcher();
        watcher.watch(&file).expect("watch file");

        watcher.poll_once();
        assert_eq!(take_events(&events), Vec::new());

        write_settled_file(&file, b"{\"a\": 1}");
        watcher.poll_once();
        assert_eq!(take_events(&events), vec![modified(file.clone())]);

        fs::remove_file(&file).expect("remove file");
        watcher.poll_once();
        assert_eq!(take_events(&events), vec![removed_file(file)]);
    }

    #[test]
    fn watching_a_missing_path_fails() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let (watcher, _events) = recording_watcher();
        let error = watcher
            .watch(&temp_dir.path().join("missing"))
            .expect_err("missing path cannot be watched");
        assert!(matches!(error.kind, notify::ErrorKind::PathNotFound));
    }

    #[test]
    fn unwatched_paths_are_not_polled() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");
        watcher.unwatch(&root).expect("unwatch directory");

        fs::write(root.join("file.txt"), b"a").expect("create file");
        watcher.poll_once();
        assert_eq!(take_events(&events), Vec::new());
    }

    #[test]
    fn a_pass_that_raced_an_unwatch_and_rewatch_reports_nothing() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let root = temp_dir.path().join("watched");
        fs::create_dir(&root).expect("create watched dir");

        let (watcher, events) = recording_watcher();
        watcher.watch(&root).expect("watch directory");
        let stale_generation = watcher
            .entries
            .lock()
            .by_path
            .get(root.as_path())
            .map(|entry| entry.generation);

        watcher.unwatch(&root).expect("unwatch directory");
        write_settled_file(&root.join("file.txt"), b"a");
        watcher.watch(&root).expect("watch directory again");

        watcher.poll_entry(&root, stale_generation, Instant::now());
        assert_eq!(take_events(&events), Vec::new());
        // The new baseline already contains the file.
        watcher.poll_once();
        assert_eq!(take_events(&events), Vec::new());
    }

    #[test]
    fn polling_a_single_path_reports_changes_since_the_last_pass() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let first = temp_dir.path().join("first");
        let second = temp_dir.path().join("second");
        fs::create_dir(&first).expect("create first dir");
        fs::create_dir(&second).expect("create second dir");

        let (watcher, events) = recording_watcher();
        watcher.watch(&first).expect("watch first");
        watcher.watch(&second).expect("watch second");

        write_settled_file(&first.join("a.txt"), b"a");
        write_settled_file(&second.join("b.txt"), b"b");
        watcher.poll_path(&first);
        assert_eq!(
            take_events(&events),
            vec![created_file(first.join("a.txt"))]
        );

        watcher.poll_once();
        assert_eq!(
            take_events(&events),
            vec![created_file(second.join("b.txt"))]
        );
    }

    #[test]
    fn budgeted_passes_sweep_round_robin_and_revisit_changed_paths_first() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = ["a", "b", "c"].map(|name| {
            let directory = temp_dir.path().join(name);
            fs::create_dir(&directory).expect("create dir");
            directory
        });
        let (watcher, events) = recording_watcher();
        for directory in &directories {
            watcher.watch(directory).expect("watch directory");
        }

        // Listing an empty directory costs two operations, so each pass sweeps one of them.
        let created = directories[2].join("first.txt");
        write_settled_file(&created, b"a");
        watcher.poll_pass(Some(2), &mut |_| {});
        watcher.poll_pass(Some(2), &mut |_| {});
        assert_eq!(take_events(&events), Vec::new());
        watcher.poll_pass(Some(2), &mut |_| {});
        assert_eq!(take_events(&events), vec![created_file(created)]);

        // The directory that just changed is polled before the sweep resumes.
        let second = directories[2].join("second.txt");
        write_settled_file(&second, b"a");
        watcher.poll_pass(Some(2), &mut |_| {});
        assert_eq!(take_events(&events), vec![created_file(second)]);
    }

    #[test]
    fn pacing_holds_the_operation_rate() {
        assert_eq!(pacing_delay(5_000, Duration::from_secs(1), 10_000), None);
        assert_eq!(
            pacing_delay(20_000, Duration::from_secs(1), 10_000),
            Some(Duration::from_secs(1))
        );
        // Tiny debts are carried over instead of turning into many short sleeps.
        assert_eq!(pacing_delay(100, Duration::ZERO, 10_000), None);
        assert_eq!(sweep_operation_budget(Duration::from_secs(2)), 20_000);
    }
}
