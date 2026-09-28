use futures::{Stream, StreamExt as _};
use gpui::{BackgroundExecutor, Task};
use notify::{Event, EventKind};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::{HashMap, hash_map::Entry},
    hash::{DefaultHasher, Hash, Hasher},
    ops::DerefMut,
    path::Path,
    pin::Pin,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use util::{ResultExt, paths::SanitizedPath};

use crate::{Fs, PathEvent, PathEventKind, Watcher};

mod diagnostics;
mod fallback_watcher;
mod watch_budget;

pub use diagnostics::{
    WatchDiagnosticEvent, WatchRecording, WatchRoot, WatchSnapshot, WatcherSnapshot,
};
use fallback_watcher::{FallbackEventSink, FallbackWatcher, PollPacer, sweep_operation_budget};
use watch_budget::{
    MAINTENANCE_INTERVAL, PROMOTION_INTERVAL, PROMOTIONS_PER_MEASUREMENT, ProcfsBudgetSource,
    WatchBudget, native_watches_over_learned_limit, native_watches_over_share, promotion_capacity,
    should_yield_native_watches,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum OsWatcherKind {
    Native,
    Poll,
    /// Non-recursive polling of a single path, which the native watcher uses in place of an
    /// inotify watch when Zed's watches would crowd out other programs running as the same user
    /// (ADR-F43). It is not a watcher of its own: it only names, in diagnostics, the paths and
    /// events of the native watcher's fallback poller.
    Fallback,
}

impl OsWatcherKind {
    /// Poll watches and native macOS/Windows watches cover their whole subtree;
    /// native Linux (inotify) watches and fallback polls cover one directory.
    fn is_recursive(self) -> bool {
        match self {
            Self::Native => cfg!(any(target_os = "windows", target_os = "macos")),
            Self::Poll => true,
            Self::Fallback => false,
        }
    }
}

pub(crate) async fn watch(
    fs: Arc<dyn Fs>,
    native_watcher: Arc<OsWatcher>,
    poll_watcher: Arc<OsWatcher>,
    executor: BackgroundExecutor,
    path: &Path,
    latency: Duration,
) -> (
    Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
    Arc<dyn Watcher>,
) {
    let (tx, rx) = async_channel::unbounded();
    let pending_paths: Arc<Mutex<Vec<PathEvent>>> = Default::default();

    let watcher: Arc<dyn Watcher> = Arc::new(FsWatcher::new(
        native_watcher,
        poll_watcher,
        fs.clone(),
        executor.clone(),
        tx,
        pending_paths.clone(),
    ));

    if let Err(e) = watcher.add(path) {
        log::warn!("Failed to watch {}:\n{e}", path.display());
    }

    // Check if path is a symlink and follow the target parent
    if let Some(mut target) = fs.read_link(path).await.ok() {
        log::trace!("watch symlink {path:?} -> {target:?}");
        // Check if symlink target is relative path, if so make it absolute
        if target.is_relative()
            && let Some(parent) = path.parent()
        {
            target = parent.join(target);
            if let Ok(canonical) = fs.canonicalize(&target).await {
                target = SanitizedPath::new(&canonical).as_path().to_path_buf();
            }
        }
        watcher.add(&target).ok();
        // Skipped for poll watchers: PollWatcher::watch() recursively scans
        // at registration, blocking on large virtual filesystem mounts
        if let Some(parent) = target.parent()
            && !fs.requires_poll_watcher(parent)
        {
            watcher.add(parent).log_err();
        }
    }

    (
        Box::pin(rx.filter_map({
            let watcher = watcher.clone();
            move |_| {
                let _ = watcher.clone();
                let pending_paths = pending_paths.clone();
                let executor = executor.clone();
                async move {
                    if !latency.is_zero() {
                        executor.timer(latency).await;
                    }
                    let paths = std::mem::take(&mut *pending_paths.lock());
                    log::debug!("pending path events: {:?}", paths);
                    (!paths.is_empty()).then_some(paths)
                }
            }
        })),
        watcher,
    )
}

pub struct FsWatcher {
    native_watcher: Arc<OsWatcher>,
    poll_watcher: Arc<OsWatcher>,
    fs: Arc<dyn Fs>,
    executor: BackgroundExecutor,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    registrations: Arc<Mutex<HashMap<WatchKey, FsWatcherRegistration>>>,
    pending_registrations: Arc<Mutex<HashMap<Arc<std::path::Path>, Task<()>>>>,
}

#[derive(Clone)]
struct FsWatcherRegistration {
    id: WatcherRegistrationId,
    os_watcher: Arc<OsWatcher>,
    /// Whether the registration covers descendants, taken when it was made. A native watcher
    /// on macOS or Windows is recursive, but a path its fallback poller holds is not; later
    /// hand-overs between the two only happen where both watch a single directory (Linux), so
    /// they don't change this.
    recursive: bool,
}

impl FsWatcher {
    pub(crate) fn new(
        native_watcher: Arc<OsWatcher>,
        poll_watcher: Arc<OsWatcher>,
        fs: Arc<dyn Fs>,
        executor: BackgroundExecutor,
        tx: async_channel::Sender<()>,
        pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    ) -> Self {
        Self {
            native_watcher,
            poll_watcher,
            fs,
            executor,
            tx,
            pending_path_events,
            registrations: Default::default(),
            pending_registrations: Default::default(),
        }
    }

    fn add_existing_path(&self, path: Arc<Path>) -> anyhow::Result<()> {
        let case_insensitive = !self.fs.is_path_case_sensitive(&path);
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);
        if self.registrations.lock().contains_key(&key) {
            log::trace!("path to watch is already watched: {path:?}");
            return Ok(());
        }
        match register_existing_path(
            &self.native_watcher,
            &self.poll_watcher,
            self.fs.as_ref(),
            path.clone(),
            case_insensitive,
            self.tx.clone(),
            self.pending_path_events.clone(),
        )? {
            Some(registration) => {
                self.registrations.lock().insert(key, registration);
            }
            None => {
                // Registration was skipped (the native watch-limit cooldown of a watcher
                // without a fallback poller is active). Retry in the background rather than
                // silently leaving the path unwatched forever. The native watcher of a real
                // filesystem has a fallback poller and never skips (ADR-F43).
                log::warn!("watch registration for {path:?} was skipped; retrying in background");
                self.add_pending_path(path);
            }
        }
        Ok(())
    }

    fn add_pending_path(&self, path: Arc<Path>) {
        let mut pending_registrations = self.pending_registrations.lock();
        if pending_registrations.contains_key(path.as_ref()) {
            return;
        }

        let task = self.executor.spawn(poll_path_until_created(
            self.native_watcher.clone(),
            self.poll_watcher.clone(),
            self.fs.clone(),
            self.executor.clone(),
            path.clone(),
            self.tx.clone(),
            self.pending_path_events.clone(),
            self.registrations.clone(),
            self.pending_registrations.clone(),
        ));
        pending_registrations.insert(path, task);
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.pending_registrations.lock().clear();

        let mut registrations = HashMap::new();
        {
            let old = &mut self.registrations.lock();
            std::mem::swap(old.deref_mut(), &mut registrations);
        }

        for (_, registration) in registrations {
            registration.os_watcher.remove(registration.id);
        }
    }
}

impl Watcher for FsWatcher {
    fn add(&self, path: &std::path::Path) -> anyhow::Result<()> {
        log::trace!("watcher add: {path:?}");

        let path: Arc<Path> = path.into();
        if path_covered_by_recursive_registration(
            &self.registrations.lock(),
            SanitizedPath::new(&path),
        ) {
            log::trace!("path to watch is covered by an existing registration: {path:?}");
            return Ok(());
        }

        if self
            .pending_registrations
            .lock()
            .contains_key(path.as_ref())
        {
            log::trace!("path to watch is already pending: {path:?}");
            return Ok(());
        }

        if !self.fs.path_exists(&path) {
            self.add_pending_path(path);
            return Ok(());
        }

        self.add_existing_path(path)
    }

    fn remove(&self, path: &std::path::Path) -> anyhow::Result<()> {
        log::trace!("remove watched path: {path:?}");
        self.pending_registrations.lock().remove(path);

        let sanitized = SanitizedPath::new(path);
        let registration = {
            let mut registrations = self.registrations.lock();
            registrations
                .remove(&WatchKey::exact(sanitized))
                .or_else(|| registrations.remove(&WatchKey::folded(sanitized)))
        };
        if let Some(registration) = registration {
            registration.os_watcher.remove(registration.id);
        }
        Ok(())
    }
}

/// Whether a recursive registration on a strict ancestor of `path` already covers
/// it. Both key spellings are probed so a folded registration still matches; only
/// poll watches and native macOS/Windows watches are recursive.
fn path_covered_by_recursive_registration(
    registrations: &HashMap<WatchKey, FsWatcherRegistration>,
    path: &SanitizedPath,
) -> bool {
    path.as_path().ancestors().skip(1).any(|ancestor| {
        let ancestor = SanitizedPath::unchecked_new(ancestor);
        [WatchKey::exact(ancestor), WatchKey::folded(ancestor)]
            .iter()
            .any(|key| {
                registrations
                    .get(key)
                    .is_some_and(|registration| registration.recursive)
            })
    })
}

/// Detect whether a path requires polling instead of native file watching.
///
/// Returns `true` for filesystem types where inotify/FSEvents/ReadDirectoryChanges
/// silently fail to deliver events: 9P (WSL drvfs), NFS, CIFS/SMB, FUSE (sshfs), etc.
/// On Windows that is the `\\wsl.localhost\<distro>\…` share; other volumes whose driver
/// has no `ReadDirectoryChangesW` are caught later, when the native watch itself fails.
///
/// Can be overridden with the `ZED_FILE_WATCHER_MODE` environment variable:
/// - `native` — always use native OS watcher
/// - `poll` — always use polling
/// - `auto` (default) — auto-detect based on filesystem type
pub fn requires_poll_watcher(path: &Path) -> bool {
    requires_poll_watcher_with_mode(
        path,
        std::env::var("ZED_FILE_WATCHER_MODE")
            .as_deref()
            .unwrap_or("auto"),
    )
}

fn requires_poll_watcher_with_mode(path: &Path, mode: &str) -> bool {
    match mode {
        "native" => return false,
        "poll" => return true,
        _ => {}
    }

    #[cfg(target_os = "linux")]
    {
        return detect_requires_poll_watcher_linux(path);
    }

    #[cfg(target_os = "windows")]
    {
        // `ReadDirectoryChangesW` fails with `ERROR_INVALID_FUNCTION` on the 9P share that
        // serves a WSL distribution's own filesystem, so a worktree opened through
        // `\\wsl.localhost\<distro>\…` would never see a change made from inside WSL
        // (ADR-F49 decision 33). Decided on the spelling alone where the spelling says it;
        // the share reached through a drive letter costs one lookup per drive, never a
        // round trip through 9P.
        return path_is_on_wsl_share(path);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        false
    }
}

/// Whether `path` is on the 9P share that serves a WSL distribution's own filesystem.
///
/// Three spellings count: `\\wsl.localhost\<distro>\…` itself; a drive letter that is assigned
/// to such a share (`net use Z: \\wsl.localhost\Ubuntu`, or a `subst` — possibly one that
/// leads there through further letters); and a path below a watch root that turned out to be a
/// junction or a directory symbolic link onto the share. A letter says nothing on its own, so
/// the assignment behind it is resolved once per letter. A reparse point says nothing either,
/// and following one costs an open, so roots are resolved by `note_watch_root` and the paths
/// below them answer from what their root resolved to.
pub fn path_is_on_wsl_share(path: &Path) -> bool {
    if util::path_mapping::is_wsl_share_path(path) {
        return true;
    }

    #[cfg(target_os = "windows")]
    {
        drive_of_path_is_wsl_share(path) || path_is_below_a_wsl_share_root(path)
    }

    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Resolve the root of a watch, so that the paths below it are decided by where the root
/// actually leads instead of by how it is spelled.
///
/// A root reached through a junction or a directory symbolic link (`C:\wsl` pointing at
/// `\\wsl.localhost\Ubuntu\home`) is on the 9P share although nothing in its spelling says so,
/// and only opening it answers that. That open is why this is a step of its own: a single
/// worktree asks `requires_poll_watcher` thousands of times, but it has one root.
pub fn note_watch_root(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        remember_watch_root(path);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
    }
}

/// `requires_poll_watcher` for a path that is the root of a watch, which is also where that
/// root is resolved (see `note_watch_root`).
pub fn requires_poll_watcher_for_root(root: &Path) -> bool {
    note_watch_root(root);
    requires_poll_watcher(root)
}

/// Where a drive letter, or a path's own spelling, leads — as far as watching what is below it
/// is concerned.
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Destination {
    /// The 9P share that serves a WSL distribution's own filesystem.
    WslShare,
    /// Some other server's share (`net use X: \\fileserver\share`, or a `subst` onto one).
    OtherShare,
    /// A volume of this machine's, or an assignment that leads nowhere this can follow.
    LocalVolume,
}

/// Where each drive letter led, so that the assignment is asked for once instead of once per
/// watch registration (a worktree registers thousands). A drive re-assigned while Zed is
/// running therefore keeps its old answer until the next start.
#[cfg(target_os = "windows")]
static DRIVE_DESTINATIONS: LazyLock<Mutex<HashMap<char, Destination>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

/// Where each watch root led once the reparse points on the way were followed, as `(the root
/// folded for comparison, whether it is on the share)`. Only `note_watch_root` adds to it, so
/// it holds one entry per watched root — a worktree, a watched configuration directory — and
/// not one per registration. Like `DRIVE_DESTINATIONS` it is never invalidated: a root that is
/// re-pointed while Zed is running keeps its answer until the next start.
#[cfg(target_os = "windows")]
static WSL_SHARE_ROOTS: LazyLock<Mutex<Vec<(String, bool)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Roots are few, so this is only a ceiling against a caller that would announce one for every
/// path it watches. Past it a root is still answered, just from its spelling and its drive
/// letter alone.
#[cfg(target_os = "windows")]
const MAX_REMEMBERED_WATCH_ROOTS: usize = 256;

#[cfg(target_os = "windows")]
fn remember_watch_root(root: &Path) {
    let key = watch_root_key(root);
    {
        let roots = WSL_SHARE_ROOTS.lock();
        if roots.iter().any(|(known, _)| *known == key) {
            return;
        }
        if roots.len() >= MAX_REMEMBERED_WATCH_ROOTS {
            log::warn!(
                "more than {MAX_REMEMBERED_WATCH_ROOTS} watch roots, so {} is decided on its spelling alone",
                root.display()
            );
            return;
        }
    }

    let is_share = root_leads_to_wsl_share(
        root,
        |path: &Path| std::fs::canonicalize(path),
        cached_drive_destination,
    );
    let mut roots = WSL_SHARE_ROOTS.lock();
    if roots.len() < MAX_REMEMBERED_WATCH_ROOTS && !roots.iter().any(|(known, _)| *known == key) {
        roots.push((key, is_share));
    }
}

/// Whether a path that is the root of a watch leads to the share. `canonicalize` follows the
/// junctions and the directory symbolic links on the way to it, and `drive_destination`
/// answers for a drive letter; both are arguments so that the decision can be tested without a
/// real reparse point and without a real assignment.
#[cfg(target_os = "windows")]
fn root_leads_to_wsl_share(
    root: &Path,
    canonicalize: impl Fn(&Path) -> std::io::Result<std::path::PathBuf>,
    drive_destination: impl Fn(char) -> Destination,
) -> bool {
    match spelled_destination(root, &drive_destination) {
        Some(Destination::WslShare) => return true,
        // A root that is already on some other server's share is not reached through a
        // junction or a directory symbolic link of this machine's — a reparse point out there
        // is resolved by that server — and opening it is a round trip to it, which a mapping
        // whose server is gone answers only after its own timeout. So it is answered here.
        Some(Destination::OtherShare) => return false,
        Some(Destination::LocalVolume) | None => {}
    }
    match canonicalize(root) {
        // `GetFinalPathNameByHandleW`, which is what `canonicalize` is here, answers where the
        // open actually landed: `\\?\UNC\wsl.localhost\…` for a link onto the share, and the
        // underlying volume's own spelling for a `subst`. So the answer is read as a path
        // again, drive letter and all.
        Ok(canonical) => {
            spelled_destination(&canonical, &drive_destination) == Some(Destination::WslShare)
        }
        Err(error) => {
            // A root that does not exist yet, or that this process cannot open, has nothing to
            // follow; its own spelling is the whole answer, and it was read above.
            log::debug!(
                "could not follow {} to decide whether it is on a WSL share: {error}",
                root.display()
            );
            false
        }
    }
}

/// Where a path's own spelling says it is, with the assignment behind its drive letter read
/// for it, or `None` when the spelling names neither a share nor a drive letter — a relative
/// path, or one under a device name.
#[cfg(target_os = "windows")]
fn spelled_destination(
    path: &Path,
    drive_destination: &impl Fn(char) -> Destination,
) -> Option<Destination> {
    if util::path_mapping::is_wsl_share_path(path) {
        return Some(Destination::WslShare);
    }
    if let Some(drive) = drive_letter_of(path) {
        return Some(drive_destination(drive));
    }
    path_names_a_share(path).then_some(Destination::OtherShare)
}

/// Whether the path is spelled as a server's share rather than as a drive letter or a device:
/// `\\<server>\<share>…`, or the `\\?\UNC\…` spelling `canonicalize` answers with.
#[cfg(target_os = "windows")]
fn path_names_a_share(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return !rest.is_empty();
    }
    // `\\?\C:\…` is a drive, and `\\.\…` names a device, although both begin with two
    // separators.
    if text.starts_with(r"\\?\") || text.starts_with(r"\\.\") {
        return false;
    }
    text.strip_prefix(r"\\")
        .or_else(|| text.strip_prefix("//"))
        .is_some_and(|rest| !rest.is_empty())
}

/// Whether `path` is below a root that `note_watch_root` resolved to the share. The nearest
/// root decides, so a root below another root can answer differently from the one above it.
#[cfg(target_os = "windows")]
fn path_is_below_a_wsl_share_root(path: &Path) -> bool {
    let roots = WSL_SHARE_ROOTS.lock();
    // This is asked of every path that is registered, and folding one costs three allocations,
    // so the roots are looked at first: with none of them on the share the answer is false
    // whichever root is nearest.
    if !roots.iter().any(|(_, is_share)| *is_share) {
        return false;
    }
    answer_of_nearest_root(&watch_root_key(path), &roots).unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn answer_of_nearest_root(key: &str, roots: &[(String, bool)]) -> Option<bool> {
    roots
        .iter()
        .filter(|(root, _)| {
            // A prefix only counts at a component boundary: `C:\wslfoo` is not below `C:\wsl`.
            key.strip_prefix(root.as_str())
                .is_some_and(|below| below.is_empty() || below.starts_with('\\'))
        })
        .max_by_key(|(root, _)| root.len())
        .map(|(_, is_share)| *is_share)
}

/// A path as roots and the paths below them are compared: without the `\\?\` prefix, with one
/// spelling of the separator, without a trailing one, and folded to lower case because Windows
/// does not tell `C:\Wsl` and `c:\wsl` apart.
#[cfg(target_os = "windows")]
fn watch_root_key(path: &Path) -> String {
    let text = SanitizedPath::new(path)
        .as_path()
        .to_string_lossy()
        .replace('/', r"\")
        .to_lowercase();
    let trimmed = text.trim_end_matches('\\');
    if trimmed.is_empty() {
        text
    } else {
        trimmed.to_owned()
    }
}

#[cfg(target_os = "windows")]
fn drive_of_path_is_wsl_share(path: &Path) -> bool {
    drive_letter_of(path)
        .is_some_and(|drive| cached_drive_destination(drive) == Destination::WslShare)
}

#[cfg(target_os = "windows")]
fn cached_drive_destination(drive: char) -> Destination {
    if let Some(known) = DRIVE_DESTINATIONS.lock().get(&drive) {
        return *known;
    }
    let destination = drive_destination(drive, resolve_drive_assignment);
    DRIVE_DESTINATIONS.lock().insert(drive, destination);
    destination
}

/// The drive letter a path starts with (`Z:\…`, `Z:/…`, `\\?\Z:\…`). A UNC path or a
/// relative path has none.
#[cfg(target_os = "windows")]
fn drive_letter_of(path: &Path) -> Option<char> {
    let text = path.to_str()?;
    let text = text.strip_prefix(r"\\?\").unwrap_or(text);
    let mut characters = text.chars();
    let letter = characters.next()?;
    if !letter.is_ascii_alphabetic() || characters.next() != Some(':') {
        return None;
    }
    match characters.next() {
        None | Some('\\') | Some('/') => Some(letter.to_ascii_uppercase()),
        // `Z:relative` is relative to that drive's current directory, not to its root.
        Some(_) => None,
    }
}

/// How many assignments are followed before giving up. `subst Z: Y:\foo` on a letter that is
/// itself assigned is legal and can be nested, but a chain this long is a mistake, and one
/// that leads back to a letter it already passed has to end somewhere.
#[cfg(target_os = "windows")]
const MAX_DRIVE_INDIRECTION: usize = 8;

/// Where what `drive` is assigned to leads. `resolve` answers with the assignment as Windows
/// spells it, which is what makes this testable without a real mapped drive.
#[cfg(target_os = "windows")]
fn drive_destination(drive: char, resolve: impl Fn(char) -> Option<String>) -> Destination {
    match unc_spelling_of_drive(drive, resolve) {
        Some(unc) if util::path_mapping::is_wsl_share_path(Path::new(&unc)) => {
            Destination::WslShare
        }
        Some(_) => Destination::OtherShare,
        None => Destination::LocalVolume,
    }
}

/// The share a drive letter leads to as `\\<server>\<share>…`, or `None` when it leads to
/// something else.
///
/// An assignment that names another letter (`subst Z: Y:\foo` answers `\??\Y:\foo`, and
/// `subst Z: Y:\` answers `\??\Y:`) is followed, carrying what is below the letter along, so
/// that a letter assigned to a share through another letter still leads to that share — and so
/// that the part below the letter can be what names the share.
#[cfg(target_os = "windows")]
fn unc_spelling_of_drive(drive: char, resolve: impl Fn(char) -> Option<String>) -> Option<String> {
    let mut drive = drive.to_ascii_uppercase();
    let mut followed: Vec<char> = Vec::new();
    let mut below_the_letter = String::new();
    for _ in 0..MAX_DRIVE_INDIRECTION {
        if followed.contains(&drive) {
            log::warn!("drive {drive}: is assigned in a loop, so where it leads is unknown");
            return None;
        }
        followed.push(drive);
        let assignment = resolve(drive)?;
        match assignment_target(&assignment)? {
            AssignmentTarget::Share(unc) => return Some(format!("{unc}{below_the_letter}")),
            AssignmentTarget::Drive { letter, below } => {
                below_the_letter.insert_str(0, &below);
                drive = letter;
            }
        }
    }
    log::warn!(
        "drive {drive}: is assigned through more than {MAX_DRIVE_INDIRECTION} letters, so where it leads is unknown"
    );
    None
}

/// What a drive letter is assigned to, as far as leading to a share is concerned.
#[cfg(target_os = "windows")]
enum AssignmentTarget {
    /// `\\<server>\<share>…`.
    Share(String),
    /// Another drive letter, and what is below it (`\foo`, or nothing at all).
    Drive { letter: char, below: String },
}

/// What an assignment points at, or `None` when it points at neither a share nor another drive
/// letter — a letter on a local volume answers `\Device\HarddiskVolume3`.
///
/// `WNetGetConnectionW` already answers `\\<server>\<share>`, but `QueryDosDeviceW` answers
/// with the device the letter points at: `\??\UNC\wsl.localhost\Ubuntu\home` for a `subst` on
/// a share, `\??\Y:\foo` for a `subst` on another letter, and
/// `\Device\<redirector>\;Z:0000000000012345\wsl.localhost\Ubuntu` for a network assignment,
/// where the device name and the `;<letter>:<id>` bookkeeping component come first.
#[cfg(target_os = "windows")]
fn assignment_target(assignment: &str) -> Option<AssignmentTarget> {
    for prefix in [r"\??\UNC\", r"\\?\UNC\"] {
        if let Some(rest) = assignment.strip_prefix(prefix) {
            return Some(AssignmentTarget::Share(format!(r"\\{rest}")));
        }
    }
    let without_prefix = assignment
        .strip_prefix(r"\??\")
        .or_else(|| assignment.strip_prefix(r"\\?\"))
        .unwrap_or(assignment);
    if let Some(target) = drive_assignment_target(without_prefix) {
        return Some(target);
    }
    if assignment.starts_with(r"\\?\") || assignment.starts_with(r"\??\") {
        return None;
    }
    if assignment.starts_with(r"\\") {
        return Some(AssignmentTarget::Share(assignment.to_owned()));
    }
    let device = assignment.strip_prefix(r"\Device\")?;
    let components: Vec<&str> = device
        .split('\\')
        .skip(1)
        .skip_while(|component| component.starts_with(';'))
        .collect();
    let [server, share, ..] = components.as_slice() else {
        return None;
    };
    (!server.is_empty() && !share.is_empty())
        .then(|| AssignmentTarget::Share(format!(r"\\{}", components.join("\\"))))
}

#[cfg(target_os = "windows")]
fn drive_assignment_target(assignment: &str) -> Option<AssignmentTarget> {
    let mut characters = assignment.chars();
    let letter = characters.next()?;
    if !letter.is_ascii_alphabetic() || characters.next() != Some(':') {
        return None;
    }
    let below = characters.as_str();
    // `Y:foo` is relative to that letter's own current directory, which is not a place this
    // can follow.
    if !(below.is_empty() || below.starts_with('\\') || below.starts_with('/')) {
        return None;
    }
    Some(AssignmentTarget::Drive {
        letter: letter.to_ascii_uppercase(),
        below: below.replace('/', r"\"),
    })
}

/// How `drive` is assigned, asked of Windows itself: `WNetGetConnectionW` for the assignments
/// a redirector serves (`net use`), and `QueryDosDeviceW` for the rest (`subst`).
#[cfg(target_os = "windows")]
fn resolve_drive_assignment(drive: char) -> Option<String> {
    network_connection_of_drive(drive).or_else(|| dos_device_of_drive(drive))
}

#[cfg(target_os = "windows")]
fn network_connection_of_drive(drive: char) -> Option<String> {
    use windows::{
        Win32::{
            Foundation::{ERROR_CONNECTION_UNAVAIL, ERROR_MORE_DATA, NO_ERROR},
            NetworkManagement::WNet::WNetGetConnectionW,
        },
        core::{HSTRING, PWSTR},
    };

    let local_name = HSTRING::from(format!("{drive}:"));
    let mut remote_name = vec![0u16; 512];
    // At most one retry: the first call reports the length it needs.
    for _ in 0..2 {
        let mut length = remote_name.len() as u32;
        let result = unsafe {
            WNetGetConnectionW(
                &local_name,
                Some(PWSTR(remote_name.as_mut_ptr())),
                &mut length,
            )
        };
        // A mapping that is only remembered — nothing has touched the letter since the
        // session began — answers `ERROR_CONNECTION_UNAVAIL` with the name it reconnects
        // to, and opening a worktree on it is what reconnects it.
        if result == NO_ERROR || result == ERROR_CONNECTION_UNAVAIL {
            return string_until_nul(&remote_name);
        }
        if result != ERROR_MORE_DATA {
            return None;
        }
        let needed = length as usize;
        if needed <= remote_name.len() || needed > 32 * 1024 {
            return None;
        }
        remote_name = vec![0u16; needed];
    }
    None
}

#[cfg(target_os = "windows")]
fn dos_device_of_drive(drive: char) -> Option<String> {
    use windows::{Win32::Storage::FileSystem::QueryDosDeviceW, core::HSTRING};

    let device_name = HSTRING::from(format!("{drive}:"));
    let mut target = vec![0u16; 1024];
    // The answer is a list of NUL-terminated names; only the first one is the assignment.
    let written = unsafe { QueryDosDeviceW(&device_name, Some(target.as_mut_slice())) };
    if written == 0 {
        return None;
    }
    string_until_nul(&target)
}

#[cfg(target_os = "windows")]
fn string_until_nul(buffer: &[u16]) -> Option<String> {
    let text: Vec<u16> = buffer
        .iter()
        .copied()
        .take_while(|unit| *unit != 0)
        .collect();
    (!text.is_empty()).then(|| String::from_utf16_lossy(&text))
}

fn register_existing_path(
    native_watcher: &Arc<OsWatcher>,
    poll_watcher: &Arc<OsWatcher>,
    fs: &dyn Fs,
    path: Arc<Path>,
    case_insensitive: bool,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
) -> anyhow::Result<Option<FsWatcherRegistration>> {
    if fs.requires_poll_watcher(path.as_ref()) {
        log::info!(
            "Using poll watcher ({}ms interval) for {}",
            poll_interval().as_millis(),
            path.display()
        );
        telemetry::event!("fs_watcher_poll", path = path.display().to_string());
        return register_with(
            poll_watcher,
            path,
            case_insensitive,
            tx,
            pending_path_events,
        );
    }
    match register_with(
        native_watcher,
        path.clone(),
        case_insensitive,
        tx.clone(),
        pending_path_events.clone(),
    ) {
        // Polling is recursive, which is what the native watch would have been on the platform
        // that reports this (Windows), so the registration still covers descendants.
        Err(error) if is_unsupported_native_watch_error(&error) => {
            log::info!(
                "Native file watching is not supported for {} ({error}); using poll watcher ({}ms interval)",
                path.display(),
                poll_interval().as_millis(),
            );
            telemetry::event!("fs_watcher_poll", path = path.display().to_string());
            register_with(
                poll_watcher,
                path,
                case_insensitive,
                tx,
                pending_path_events,
            )
        }
        result => result,
    }
}

fn register_with(
    os_watcher: &Arc<OsWatcher>,
    path: Arc<Path>,
    case_insensitive: bool,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
) -> anyhow::Result<Option<FsWatcherRegistration>> {
    let root_path = SanitizedPath::new_arc(path.as_ref());
    let path_for_callback = path.clone();
    let Some(registration_id) =
        os_watcher.add(path, case_insensitive, move |event: &notify::Event| {
            log::trace!("watcher received event: {event:?}");
            push_notify_event(
                &tx,
                &pending_path_events,
                &root_path,
                case_insensitive,
                path_for_callback.as_ref(),
                event,
            );
        })?
    else {
        return Ok(None);
    };
    Ok(Some(FsWatcherRegistration {
        id: registration_id,
        recursive: os_watcher.covers_descendants(registration_id),
        os_watcher: os_watcher.clone(),
    }))
}

#[cfg(target_os = "linux")]
fn detect_requires_poll_watcher_linux(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return false;
    }

    const V9FS_MAGIC: u64 = 0x0102_1997;
    const NFS_SUPER_MAGIC: u64 = 0x0000_6969;
    const CIFS_MAGIC: u64 = 0xFF53_4D42;
    const SMB_SUPER_MAGIC: u64 = 0x0000_517B;
    const SMB2_MAGIC: u64 = 0xFE53_4D42;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

    let fs_type = (stat.f_type as u64) & 0xFFFF_FFFF;
    if fs_type == FUSE_SUPER_MAGIC && is_virtiofs(path) {
        return false;
    }

    if fs_type == V9FS_MAGIC
        || fs_type == NFS_SUPER_MAGIC
        || fs_type == CIFS_MAGIC
        || fs_type == SMB_SUPER_MAGIC
        || fs_type == SMB2_MAGIC
        || fs_type == FUSE_SUPER_MAGIC
    {
        log::info!(
            "Detected network/virtual filesystem (type 0x{:x}) at {}, using poll watcher",
            fs_type,
            path.display()
        );
        return true;
    }

    if is_wsl_drvfs_path(path) {
        log::info!(
            "Detected WSL drvfs mount at {}, using poll watcher",
            path.display()
        );
        return true;
    }

    false
}

#[cfg(target_os = "linux")]
fn is_virtiofs(path: &Path) -> bool {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };

    let mut best_mount = None;
    for line in mountinfo.lines() {
        let fields = line.split(' ').collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let (Some(mount_point), Some(fs_type)) = (fields.get(4), fields.get(separator + 1)) else {
            continue;
        };

        let mount_point = mount_point
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if path.starts_with(&mount_point)
            && best_mount.is_none_or(|(length, _)| mount_point.len() > length)
        {
            best_mount = Some((mount_point.len(), *fs_type));
        }
    }

    best_mount.is_some_and(|(_, fs_type)| fs_type == "virtiofs" || fs_type == "fuse.virtiofs")
}

#[cfg(target_os = "linux")]
fn is_wsl_drvfs_path(path: &Path) -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        if let Ok(version) = std::fs::read_to_string("/proc/version") {
            let version = version.to_lowercase();
            if !version.contains("microsoft") && !version.contains("wsl") {
                return false;
            }
        } else {
            return false;
        }
    }

    let Some(path) = path.to_str() else {
        return false;
    };
    if !path.starts_with("/mnt/") || path.len() < 6 {
        return false;
    }
    let after_mnt = &path[5..];
    after_mnt.starts_with(|c: char| c.is_ascii_alphabetic())
        && (after_mnt.len() == 1 || after_mnt.as_bytes()[1] == b'/')
}

/// Whether the volume backing `path` does case-insensitive name lookups, used to
/// pick exact vs. folded matching.
#[cfg(target_os = "macos")]
pub(crate) fn case_insensitive_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    // `pathconf(_PC_CASE_SENSITIVE)` returns 1 (sensitive), 0 (insensitive), or -1
    // on error; default errors to insensitive (the APFS/HFS+ default).
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: We just initialized c_path, so it's a valid pointer
    unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_CASE_SENSITIVE) == 0 }
}

#[cfg(target_os = "linux")]
pub(crate) fn case_insensitive_path(_path: &Path) -> bool {
    // use std::os::unix::ffi::OsStrExt as _;

    // // Only ext4/f2fs casefold (`+F`) dirs are insensitive, reported by `statx` via
    // // STATX_ATTR_CASEFOLD; any failure (e.g. pre-4.11 ENOSYS) means case-sensitive.
    // const STATX_ATTR_CASEFOLD: u64 = 0x0000_2000;
    // let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
    //     return false;
    // };
    // let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();

    // // SAFETY: c_path is still valid, buffer has been zeroed
    // if unsafe { libc::statx(libc::AT_FDCWD, c_path.as_ptr(), 0, 0, buf.as_mut_ptr()) } != 0 {
    //     return false;
    // }

    // // SAFETY: libc statx initialized this buffer, otherwise we would've returned on a error
    // // in that function call
    // let buf = unsafe { buf.assume_init() };
    // buf.stx_attributes_mask & STATX_ATTR_CASEFOLD != 0
    //     && buf.stx_attributes & STATX_ATTR_CASEFOLD != 0
    false
}

#[cfg(target_os = "windows")]
pub(crate) fn case_insensitive_path(_path: &Path) -> bool {
    // todo(windows): Windows defaults to case in sensitive, but
    // they can mark specific directories as case sensitive. Mainly
    // for WSL use cases
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub(crate) fn case_insensitive_path(_path: &Path) -> bool {
    // Other BSDs default to case-sensitive local filesystems.
    false
}

/// Whether `path` is `root` or sits beneath it, folding case on case-insensitive
/// volumes so a differently-cased spelling still matches.
fn path_is_under(path: &SanitizedPath, root: &SanitizedPath, case_insensitive: bool) -> bool {
    if case_insensitive {
        let path = path.as_path().to_string_lossy().to_lowercase();
        let root = root.as_path().to_string_lossy().to_lowercase();
        Path::new(&path).starts_with(Path::new(&root))
    } else {
        path.starts_with(root)
    }
}

/// Lookup key for a watch path, shared by add, remove, and dispatch so they all
/// agree on whether two spellings denote the same directory.
///
/// On case-sensitive volumes the exact (sanitized) path is the key, so genuinely
/// distinct directories stay distinct. On case-insensitive volumes the folded
/// (lowercased) spelling is the key, so any casing of a directory collides.
///
/// The two variants are distinct map keys, so a case-sensitive registration can
/// never be hit by a folded lookup (or vice versa); dispatch can therefore probe
/// both forms of an event path without risking a cross-rule false match.
///
/// NOTE: folding only normalizes case. macOS (APFS/HFS+) is also Unicode
/// normalization-insensitive (NFC vs NFD); that normalization is intentionally
/// centralized here so it can be added in one place later without touching the
/// add/remove/dispatch call sites.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WatchKey {
    Exact(Arc<Path>),
    Folded(Arc<str>),
}

impl WatchKey {
    fn exact(path: &SanitizedPath) -> Self {
        Self::Exact(Arc::from(path.as_path()))
    }

    fn folded(path: &SanitizedPath) -> Self {
        let lossy = path.as_path().to_string_lossy();
        // macOS (APFS/HFS+) compares names normalization-insensitively (NFC vs
        // NFD), and FSEvents can report NFD while a config/LSP supplies NFC, so
        // normalize before folding case. Windows (NTFS) and Linux are
        // normalization-sensitive, so there we only fold case.
        #[cfg(target_os = "macos")]
        let folded = {
            use unicode_normalization::UnicodeNormalization as _;
            lossy.chars().nfc().collect::<String>().to_lowercase()
        };
        #[cfg(not(target_os = "macos"))]
        let folded = lossy.to_lowercase();
        Self::Folded(folded.into())
    }

    fn for_registration(path: &SanitizedPath, case_insensitive: bool) -> Self {
        if case_insensitive {
            Self::folded(path)
        } else {
            Self::exact(path)
        }
    }
}

async fn poll_path_until_created(
    native_watcher: Arc<OsWatcher>,
    poll_watcher: Arc<OsWatcher>,
    fs: Arc<dyn Fs>,
    executor: BackgroundExecutor,
    path: Arc<Path>,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    registrations: Arc<Mutex<HashMap<WatchKey, FsWatcherRegistration>>>,
    pending_registrations: Arc<Mutex<HashMap<Arc<Path>, Task<()>>>>,
) {
    loop {
        executor.timer(poll_interval()).await;

        if !pending_registrations.lock().contains_key(path.as_ref()) {
            return;
        }

        if !fs.path_exists(&path) {
            continue;
        }

        // Probe case sensitivity now that the path exists, rather than at add
        // time when it didn't.
        let case_insensitive = !fs.is_path_case_sensitive(&path);
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);

        if registrations.lock().contains_key(&key) {
            pending_registrations.lock().remove(path.as_ref());
            return;
        }

        match register_existing_path(
            &native_watcher,
            &poll_watcher,
            fs.as_ref(),
            path.clone(),
            case_insensitive,
            tx.clone(),
            pending_path_events.clone(),
        ) {
            Ok(Some(registration)) => {
                {
                    let mut pending_registrations = pending_registrations.lock();
                    if pending_registrations.remove(path.as_ref()).is_none() {
                        registration.os_watcher.remove(registration.id);
                        return;
                    }
                    registrations.lock().insert(key, registration);
                }
                enqueue_path_events(
                    &tx,
                    &pending_path_events,
                    vec![
                        PathEvent {
                            path: path.to_path_buf(),
                            kind: Some(PathEventKind::Created),
                        },
                        PathEvent {
                            path: path.to_path_buf(),
                            kind: Some(PathEventKind::Rescan),
                        },
                    ],
                );
                return;
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!("failed to watch newly-created path {path:?}: {error}; retrying");
            }
        }
    }
}

fn enqueue_path_events(
    tx: &smol::channel::Sender<()>,
    pending_path_events: &Arc<Mutex<Vec<PathEvent>>>,
    mut path_events: Vec<PathEvent>,
) {
    if path_events.is_empty() {
        return;
    }

    path_events.sort();
    let mut pending_paths = pending_path_events.lock();
    if pending_paths.is_empty() {
        tx.try_send(()).ok();
    }
    coalesce_pending_rescans(&mut pending_paths, &mut path_events);
    util::extend_sorted(&mut *pending_paths, path_events, usize::MAX, |a, b| {
        a.path.cmp(&b.path)
    });
}

fn push_notify_event(
    tx: &smol::channel::Sender<()>,
    pending_path_events: &Arc<Mutex<Vec<PathEvent>>>,
    root_path: &SanitizedPath,
    case_insensitive: bool,
    watched_root: &Path,
    event: &notify::Event,
) {
    let kind = match event.kind {
        EventKind::Create(_) => Some(PathEventKind::Created),
        EventKind::Modify(_) => Some(PathEventKind::Changed),
        EventKind::Remove(_) => Some(PathEventKind::Removed),
        _ => None,
    };
    let mut path_events = event
        .paths
        .iter()
        .filter_map(|event_path| {
            let event_path = SanitizedPath::new(event_path);
            path_is_under(event_path, root_path, case_insensitive).then(|| PathEvent {
                path: event_path.as_path().to_path_buf(),
                kind,
            })
        })
        .collect::<Vec<_>>();

    if event.need_rescan() {
        // A rescan event names the subtrees that lost sync (FSEvents
        // `MustScanSubDirs`); only a pathless one (inotify queue overflow) means
        // the whole watched root is suspect.
        if path_events.is_empty() {
            path_events.push(PathEvent {
                path: watched_root.to_path_buf(),
                kind: Some(PathEventKind::Rescan),
            });
        } else {
            for path_event in &mut path_events {
                path_event.kind = Some(PathEventKind::Rescan);
            }
        }
    }
    log::trace!("path_events: {:?}", path_events);
    enqueue_path_events(tx, pending_path_events, path_events);
}

fn coalesce_pending_rescans(pending_paths: &mut Vec<PathEvent>, path_events: &mut Vec<PathEvent>) {
    if !path_events
        .iter()
        .any(|event| event.kind == Some(PathEventKind::Rescan))
    {
        return;
    }

    let mut new_rescan_paths: Vec<std::path::PathBuf> = path_events
        .iter()
        .filter(|e| e.kind == Some(PathEventKind::Rescan))
        .map(|e| e.path.clone())
        .collect();
    new_rescan_paths.sort_unstable();

    let mut deduped_rescans: Vec<std::path::PathBuf> = Vec::with_capacity(new_rescan_paths.len());
    for path in new_rescan_paths {
        if deduped_rescans
            .iter()
            .any(|ancestor| path != *ancestor && path.starts_with(ancestor))
        {
            continue;
        }
        deduped_rescans.push(path);
    }

    deduped_rescans.retain(|new_path| {
        !pending_paths
            .iter()
            .any(|pending| is_covered_rescan(pending.kind, new_path, &pending.path))
    });

    if !deduped_rescans.is_empty() {
        pending_paths.retain(|pending| {
            !deduped_rescans.iter().any(|rescan_path| {
                pending.path == *rescan_path
                    || is_covered_rescan(pending.kind, &pending.path, rescan_path)
            })
        });
    }

    path_events.retain(|event| {
        event.kind != Some(PathEventKind::Rescan) || deduped_rescans.contains(&event.path)
    });
}

fn is_covered_rescan(kind: Option<PathEventKind>, path: &Path, ancestor: &Path) -> bool {
    kind == Some(PathEventKind::Rescan) && path != ancestor && path.starts_with(ancestor)
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct WatcherRegistrationId(u32);

struct WatcherRegistrationState {
    callback: Arc<dyn Fn(&notify::Event) + Send + Sync>,
    key: WatchKey,
    path: Arc<SanitizedPath>,
}

struct PathRegistrationState {
    watcher_ids: Vec<WatcherRegistrationId>,
    has_os_watcher: bool,
    /// The spelling the backend watch was created with. Unwatching the path, or handing it
    /// between the native backend and the fallback poller, has to use the same one (on a
    /// case-insensitive volume a later registration may spell it differently).
    watched_path: Arc<SanitizedPath>,
    /// Whether the native watcher's fallback poller holds this path in place of the backend
    /// (ADR-F43). A path is only ever held by one of the two, and handing it over keeps the
    /// entry and its registrations, so events that either of them queued before a hand-over
    /// still reach the same callbacks.
    polled_by_fallback: bool,
    /// Set when a native watch failed for a reason other than the watch limit, so promotion
    /// stops retrying it.
    native_unavailable: bool,
}

// Deep directories are handed to the poller first and shallow ones taken back first, so the
// directories nearest the roots keep their native watches the longest.
#[derive(Clone, Copy)]
enum TransitionOrder {
    DeepestFirst,
    ShallowestFirst,
}

/// The registered watch paths for one backend, keyed by [`WatchKey`] so that
/// add (dedup), remove, and dispatch share a single notion of path identity.
#[derive(Default)]
struct WatchPaths {
    entries: HashMap<WatchKey, PathRegistrationState>,
    /// Lets dispatch skip building folded keys while no case-insensitive path is registered,
    /// which on Linux is always (the fallback poller can hold tens of thousands of paths).
    folded_entries: usize,
}

impl WatchPaths {
    fn contains(&self, key: &WatchKey) -> bool {
        self.entries.contains_key(key)
    }

    fn get_mut(&mut self, key: &WatchKey) -> Option<&mut PathRegistrationState> {
        self.entries.get_mut(key)
    }

    fn add_watcher(
        &mut self,
        key: WatchKey,
        id: WatcherRegistrationId,
        new_entry: impl FnOnce() -> PathRegistrationState,
    ) {
        match self.entries.entry(key) {
            Entry::Occupied(mut occupied) => occupied.get_mut().watcher_ids.push(id),
            Entry::Vacant(vacant) => {
                if matches!(vacant.key(), WatchKey::Folded(_)) {
                    self.folded_entries += 1;
                }
                vacant.insert(new_entry());
            }
        }
    }

    fn remove(&mut self, key: &WatchKey) -> Option<PathRegistrationState> {
        let removed = self.entries.remove(key);
        if removed.is_some() && matches!(key, WatchKey::Folded(_)) {
            self.folded_entries = self.folded_entries.saturating_sub(1);
        }
        removed
    }

    /// The spelling of a path that holds a watch of its own, if it is held by the backend
    /// (`polled_by_fallback == false`) or by the fallback poller (`true`) as asked.
    fn watched_path(&self, key: &WatchKey, polled_by_fallback: bool) -> Option<Arc<SanitizedPath>> {
        self.entries
            .get(key)
            .filter(|registration| {
                registration.has_os_watcher && registration.polled_by_fallback == polled_by_fallback
            })
            .map(|registration| registration.watched_path.clone())
    }

    fn set_polled_by_fallback(&mut self, key: &WatchKey, polled_by_fallback: bool) {
        if let Some(registration) = self.entries.get_mut(key) {
            registration.polled_by_fallback = polled_by_fallback;
        }
    }

    fn mark_native_unavailable(&mut self, key: &WatchKey) {
        if let Some(registration) = self.entries.get_mut(key) {
            registration.native_unavailable = true;
        }
    }

    /// How many paths hold a watch of their own with the backend or with the fallback poller.
    fn count_watched(&self, polled_by_fallback: bool) -> usize {
        self.entries
            .values()
            .filter(|registration| {
                registration.has_os_watcher && registration.polled_by_fallback == polled_by_fallback
            })
            .count()
    }

    fn transition_candidates(
        &self,
        polled_by_fallback: bool,
        order: TransitionOrder,
    ) -> Vec<WatchKey> {
        let mut candidates = self
            .entries
            .iter()
            .filter(|(_, registration)| {
                registration.has_os_watcher
                    && registration.polled_by_fallback == polled_by_fallback
                    && !registration.native_unavailable
            })
            .map(|(key, registration)| {
                (
                    registration.watched_path.as_path().components().count(),
                    registration.watched_path.clone(),
                    key.clone(),
                )
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|(left_depth, left_path, _), (right_depth, right_path, _)| {
            left_depth
                .cmp(right_depth)
                .then_with(|| left_path.as_path().cmp(right_path.as_path()))
        });
        if matches!(order, TransitionOrder::DeepestFirst) {
            candidates.reverse();
        }
        candidates.into_iter().map(|(_, _, key)| key).collect()
    }

    /// True if a recursive registration on a strict ancestor already covers
    /// `path`. Only poll watches and native macOS/Windows watches are recursive,
    /// and a path the fallback poller holds covers nothing below it.
    fn covered_by_recursive_ancestor(&self, path: &SanitizedPath, recursive: bool) -> bool {
        if !recursive {
            return false;
        }
        path.as_path().ancestors().skip(1).any(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            [WatchKey::exact(ancestor), WatchKey::folded(ancestor)]
                .iter()
                .any(|key| {
                    self.entries
                        .get(key)
                        .is_some_and(|registration| !registration.polled_by_fallback)
                })
        })
    }

    /// Collects the watcher ids of every registration whose directory is an
    /// ancestor of (or equal to) `path`. Both exact and folded keys are probed,
    /// so a real-cased event path matches a folded registration and vice versa.
    fn watcher_ids_covering(&self, path: &SanitizedPath, ids: &mut Vec<WatcherRegistrationId>) {
        let include_folded = self.folded_entries > 0;
        for ancestor in path.as_path().ancestors() {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            if let Some(registration) = self.entries.get(&WatchKey::exact(ancestor)) {
                ids.extend_from_slice(&registration.watcher_ids);
            }
            if include_folded
                && let Some(registration) = self.entries.get(&WatchKey::folded(ancestor))
            {
                ids.extend_from_slice(&registration.watcher_ids);
            }
        }
    }
}

struct RemovedWatch {
    watched_path: Arc<SanitizedPath>,
    polled_by_fallback: bool,
}

struct WatcherState {
    watchers: HashMap<WatcherRegistrationId, WatcherRegistrationState>,
    paths: WatchPaths,
    cooldown_until: Option<Instant>,
    last_registration: WatcherRegistrationId,
}

impl WatcherState {
    fn new() -> Self {
        Self {
            watchers: Default::default(),
            paths: Default::default(),
            cooldown_until: None,
            last_registration: Default::default(),
        }
    }

    fn is_native_watch_limit_cooldown_active(&self) -> bool {
        self.cooldown_until
            .is_some_and(|cooldown_until| cooldown_until > Instant::now())
    }

    /// Whether a new registration on `key` can join what is already there without a backend
    /// call, and if so whether a new path entry would own a watch: either the path is
    /// registered already, or a recursive ancestor covers it.
    fn joinable_registration(
        &self,
        key: &WatchKey,
        path: &SanitizedPath,
        recursive: bool,
    ) -> Option<bool> {
        if self.paths.contains(key) {
            return Some(true);
        }
        self.paths
            .covered_by_recursive_ancestor(path, recursive)
            .then_some(false)
    }

    fn register_watcher(
        &mut self,
        key: WatchKey,
        path: Arc<SanitizedPath>,
        callback: Arc<dyn Fn(&notify::Event) + Send + Sync>,
        has_os_watcher: bool,
        polled_by_fallback: bool,
    ) -> WatcherRegistrationId {
        let id = self.last_registration;
        self.last_registration = WatcherRegistrationId(id.0 + 1);
        self.watchers.insert(
            id,
            WatcherRegistrationState {
                callback,
                key: key.clone(),
                path: path.clone(),
            },
        );
        self.paths.add_watcher(key, id, || PathRegistrationState {
            watcher_ids: vec![id],
            has_os_watcher,
            watched_path: path,
            polled_by_fallback,
            native_unavailable: false,
        });
        id
    }

    fn remove_registration(&mut self, id: WatcherRegistrationId) -> Option<RemovedWatch> {
        let registration_state = self.watchers.remove(&id)?;
        let path_state = self.paths.get_mut(&registration_state.key)?;
        path_state.watcher_ids.retain(|&existing| existing != id);
        if !path_state.watcher_ids.is_empty() {
            return None;
        }

        let path_state = self.paths.remove(&registration_state.key)?;
        path_state.has_os_watcher.then_some(RemovedWatch {
            watched_path: path_state.watched_path,
            polled_by_fallback: path_state.polled_by_fallback,
        })
    }
}

pub(crate) trait WatchBackend: Send {
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> notify::Result<()>;
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

impl<T: notify::Watcher + Send> WatchBackend for T {
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> notify::Result<()> {
        notify::Watcher::watch(self, path, mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        notify::Watcher::unwatch(self, path)
    }
}

const KEY_LOCK_STRIPES: usize = 64;

pub struct OsWatcher {
    kind: OsWatcherKind,
    recursive: bool,
    state: Arc<Mutex<WatcherState>>,

    // Never hold the state lock while calling the backend: a backend call can be
    // slow (a poll watch scans its whole tree up front, an FSEvents watch rebuilds
    // the stream) and dispatch needs the state to route every other registration's
    // events. Whoever re-locks the state afterwards must re-check what they assumed.
    backend: Mutex<Option<Box<dyn WatchBackend>>>,
    event_tx: async_channel::Sender<notify::Result<notify::Event>>,
    _dispatch_task: Task<()>,
    diagnostics: Arc<diagnostics::DiagnosticRecorder>,
    // Adding, removing, and handing a path between the backend and the fallback poller each
    // span several state updates with backend calls in between, so these operations are
    // serialized per watch key. The locks are striped so that unrelated paths don't wait on
    // each other's backend calls. A key lock is taken before the state, backend, and budget
    // locks, and no thread ever holds two of them at once.
    key_locks: [Mutex<()>; KEY_LOCK_STRIPES],
    /// Only the native watcher of a real filesystem has one (ADR-F43).
    fallback: Option<NativeFallback>,
}

/// What lets the native watcher stay within Zed's share of the user's inotify watches: paths
/// beyond the share, or refused with ENOSPC, are polled without recursion instead, which is
/// what a single inotify watch on them would report. They are handed back and forth with the
/// backend as other programs take and free watches.
///
/// It lives inside the native watcher rather than beside it as a watcher of its own because a
/// hand-over has to keep every registration's id and callback: the path stays one entry of one
/// registration table, and only who watches it changes.
struct NativeFallback {
    poller: FallbackWatcher,
    /// Shared by every native watcher of the process (see `process_watch_budget`).
    budget: Arc<WatchBudget>,
    background: Arc<FallbackBackground>,
    spawn_background_thread: bool,
    this: Weak<OsWatcher>,
}

struct FallbackOptions {
    budget: Arc<WatchBudget>,
    spawn_background_thread: bool,
}

/// The thread that polls the fallback paths and keeps the watch budget up to date.
struct FallbackBackground {
    thread: Mutex<BackgroundThread>,
    signal: Mutex<MaintenanceSignal>,
    wake: Condvar,
}

#[derive(Default)]
struct MaintenanceSignal {
    requested: bool,
    shut_down: bool,
}

enum MaintenanceWake {
    Deadline,
    Requested,
    ShutDown,
}

impl FallbackBackground {
    fn new() -> Self {
        Self {
            thread: Mutex::new(BackgroundThread::NotStarted),
            signal: Mutex::new(MaintenanceSignal::default()),
            wake: Condvar::new(),
        }
    }

    /// Asks the background thread to run maintenance right away.
    fn request_maintenance(&self) {
        self.signal.lock().requested = true;
        self.wake.notify_one();
    }

    fn take_maintenance_request(&self) -> bool {
        std::mem::replace(&mut self.signal.lock().requested, false)
    }

    fn shut_down(&self) {
        self.signal.lock().shut_down = true;
        self.wake.notify_all();
    }

    fn wait(&self, deadline: Instant) -> MaintenanceWake {
        let mut signal = self.signal.lock();
        if !signal.requested && !signal.shut_down {
            self.wake.wait_until(&mut signal, deadline);
        }
        if signal.shut_down {
            MaintenanceWake::ShutDown
        } else if std::mem::replace(&mut signal.requested, false) {
            MaintenanceWake::Requested
        } else {
            MaintenanceWake::Deadline
        }
    }
}

enum BackgroundThread {
    NotStarted,
    Running,
    FailedAt(Instant),
}

const BACKGROUND_THREAD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum WatchOutcome {
    Backend,
    Fallback,
    /// The native watch-limit cooldown of a watcher without a fallback poller is active.
    Skipped,
}

struct NewWatch {
    outcome: anyhow::Result<WatchOutcome>,
    hit_watch_limit: bool,
}

enum PromotionOutcome {
    Promoted,
    Skipped,
    BudgetExhausted,
    WatchLimitReached(Arc<SanitizedPath>),
}

struct MaintenanceSchedule {
    next_tick: Instant,
    next_promotion: Instant,
}

/// Spreads the promotion rounds of several Zed processes of the same user apart, so they don't
/// all promote from the same measurement.
fn promotion_jitter() -> Duration {
    let nanoseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let seed = u64::from(nanoseconds) ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9);
    Duration::from_millis(seed % 10_000)
}

/// The budget of every native watcher of this process. The inotify watches it holds against
/// the user's limit are the whole process's, however many filesystems (and so native watchers)
/// it has, so they are counted in one place. Several watchers trimming against the same count
/// can hand over more paths than needed; the next promotion round takes the extra back.
fn process_watch_budget() -> Arc<WatchBudget> {
    static BUDGET: LazyLock<Arc<WatchBudget>> = LazyLock::new(|| {
        // Only inotify charges one watch per directory against a small per-user limit;
        // FSEvents and ReadDirectoryChangesW watch whole trees with one handle.
        Arc::new(WatchBudget::new(
            cfg!(target_os = "linux"),
            Arc::new(ProcfsBudgetSource),
        ))
    });
    BUDGET.clone()
}

impl OsWatcher {
    /// Watches with notify's backend of `kind`, created lazily on first use so
    /// that a failure to start it can be reported by `ensure_backend`.
    pub fn new(kind: OsWatcherKind, executor: BackgroundExecutor) -> Arc<Self> {
        let fallback = (kind == OsWatcherKind::Native).then(|| FallbackOptions {
            budget: process_watch_budget(),
            spawn_background_thread: true,
        });
        Self::build(kind, executor, None, fallback)
    }

    /// Watches with the given backend in place of notify's; `event_sink` is how
    /// that backend reports events, standing in for notify's `EventHandler`.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_backend(
        kind: OsWatcherKind,
        executor: BackgroundExecutor,
        backend: Option<Box<dyn WatchBackend>>,
    ) -> Arc<Self> {
        Self::build(kind, executor, backend, None)
    }

    fn build(
        kind: OsWatcherKind,
        executor: BackgroundExecutor,
        backend: Option<Box<dyn WatchBackend>>,
        fallback: Option<FallbackOptions>,
    ) -> Arc<Self> {
        let (event_tx, event_rx) = async_channel::unbounded();
        let state = Arc::new(Mutex::new(WatcherState::new()));
        let dispatch_task = executor.spawn({
            let state = state.clone();
            async move {
                let mut rescan_history = diagnostics::RescanHistory::default();
                while let Ok(first) = event_rx.recv().await {
                    dispatch_batch(kind, &state, &mut rescan_history, first, &event_rx);
                }
            }
        });
        let diagnostics: Arc<diagnostics::DiagnosticRecorder> = Default::default();
        Arc::new_cyclic(|this| Self {
            kind,
            recursive: kind.is_recursive(),
            state,
            backend: Mutex::new(backend),
            fallback: fallback.map(|options| {
                NativeFallback::new(
                    fallback_event_sink(&event_tx, &diagnostics),
                    options,
                    this.clone(),
                )
            }),
            event_tx,
            _dispatch_task: dispatch_task,
            diagnostics,
            key_locks: std::array::from_fn(|_| Mutex::new(())),
        })
    }

    /// The callback a substitute backend uses to report events, standing
    /// in for the `EventHandler` notify would have been constructed with.
    pub(crate) fn event_sink(
        &self,
    ) -> impl Fn(notify::Result<notify::Event>) + Send + Sync + 'static {
        let event_tx = self.event_tx.clone();
        let diagnostics = self.diagnostics.clone();
        let kind = self.kind;
        move |event| {
            diagnostics.record(|| match &event {
                Ok(event) => WatchDiagnosticEvent::from_notify_event(kind, event),
                Err(error) => WatchDiagnosticEvent::new(
                    kind,
                    "error",
                    &error.paths,
                    format!("{error:?}"),
                    false,
                ),
            });
            enqueue(&event_tx, event);
        }
    }

    pub(crate) fn is_recursive(&self) -> bool {
        self.recursive
    }

    /// Whether the registration covers the descendants of its path: a path the fallback poller
    /// holds covers only itself, even where the backend's watches are recursive.
    fn covers_descendants(&self, id: WatcherRegistrationId) -> bool {
        if !self.is_recursive() {
            return false;
        }
        let state = self.state.lock();
        state
            .watchers
            .get(&id)
            .and_then(|registration| state.paths.entries.get(&registration.key))
            .is_some_and(|path| !path.polled_by_fallback)
    }

    fn lock_key(&self, key: &WatchKey) -> parking_lot::MutexGuard<'_, ()> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let stripe = (hasher.finish() % KEY_LOCK_STRIPES as u64) as usize;
        self.key_locks[stripe].lock()
    }

    #[must_use]
    fn add(
        &self,
        path: Arc<std::path::Path>,
        case_insensitive: bool,
        cb: impl Fn(&notify::Event) + Send + Sync + 'static,
    ) -> anyhow::Result<Option<WatcherRegistrationId>> {
        let path = SanitizedPath::from_arc(path);
        let key = WatchKey::for_registration(&path, case_insensitive);
        let callback: Arc<dyn Fn(&notify::Event) + Send + Sync> = Arc::new(cb);
        if let Some(fallback) = &self.fallback {
            fallback.budget.ensure_initial_measurement();
        }

        let key_guard = self.lock_key(&key);
        {
            let mut state = self.state.lock();
            if let Some(has_os_watcher) = state.joinable_registration(&key, &path, self.recursive) {
                return Ok(Some(state.register_watcher(
                    key,
                    path,
                    callback,
                    has_os_watcher,
                    false,
                )));
            }
        }

        let NewWatch {
            outcome,
            hit_watch_limit,
        } = self.watch_new_path(&path);
        let registered_outcome = outcome.as_ref().ok().copied();
        let registration = outcome.map(|outcome| {
            let polled_by_fallback = match outcome {
                WatchOutcome::Skipped => return None,
                WatchOutcome::Backend => false,
                WatchOutcome::Fallback => true,
            };
            Some(self.state.lock().register_watcher(
                key,
                path.clone(),
                callback,
                true,
                polled_by_fallback,
            ))
        });
        drop(key_guard);

        let Some(fallback) = &self.fallback else {
            return registration;
        };
        // Learning scans `/proc`, and trimming to the learned limit can move thousands of
        // paths, so neither happens under the key lock or on the caller's thread.
        if hit_watch_limit
            && fallback.budget.is_enabled()
            && fallback
                .budget
                .learn_limit_from_watch_limit_error(path.as_path())
        {
            fallback.background.request_maintenance();
        }
        let needs_background_thread = match registered_outcome {
            Some(WatchOutcome::Fallback) => true,
            Some(WatchOutcome::Backend) => fallback.budget.is_enabled(),
            Some(WatchOutcome::Skipped) | None => false,
        };
        if needs_background_thread {
            self.ensure_background_thread();
        }
        registration
    }

    /// Starts watching a path that has no registration yet. With a fallback poller, paths
    /// that can't get a native watch are polled instead of being skipped, so they never have
    /// to be retried (ADR-F43).
    fn watch_new_path(&self, path: &SanitizedPath) -> NewWatch {
        let Some(fallback) = &self.fallback else {
            if self.kind == OsWatcherKind::Native
                && self.state.lock().is_native_watch_limit_cooldown_active()
            {
                self.diagnostics.record(|| {
                    WatchDiagnosticEvent::new(
                        self.kind,
                        "watch_skipped",
                        &[path.as_path().to_owned()],
                        "Native watch limit cooldown is active; registration will be retried"
                            .into(),
                        false,
                    )
                });
                return NewWatch {
                    outcome: Ok(WatchOutcome::Skipped),
                    hit_watch_limit: false,
                };
            }
            let outcome = match self.watch(path.as_path()) {
                Ok(()) => Ok(WatchOutcome::Backend),
                Err(error)
                    if self.kind == OsWatcherKind::Native && is_max_files_watch_error(&error) =>
                {
                    self.start_native_watch_limit_cooldown(path.as_path());
                    Ok(WatchOutcome::Skipped)
                }
                Err(error) => Err(error),
            };
            return NewWatch {
                outcome,
                hit_watch_limit: false,
            };
        };

        if !fallback.budget.try_reserve_native() {
            return NewWatch {
                outcome: self.watch_fallback(fallback, path),
                hit_watch_limit: false,
            };
        }

        // Creating the inotify instance fails once the user's instance limit is reached; the
        // path is still polled then.
        if let Err(error) = self.ensure_backend() {
            fallback.budget.release_native();
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                log::warn!("native file watching is unavailable, polling instead: {error:#}");
            }
            return NewWatch {
                outcome: self.watch_fallback(fallback, path),
                hit_watch_limit: false,
            };
        }

        match self.watch(path.as_path()) {
            Ok(()) => NewWatch {
                outcome: Ok(WatchOutcome::Backend),
                hit_watch_limit: false,
            },
            Err(error) if is_max_files_watch_error(&error) => {
                fallback.budget.release_native();
                NewWatch {
                    outcome: self.watch_fallback(fallback, path),
                    hit_watch_limit: true,
                }
            }
            // Includes a volume without native watching, which `register_existing_path`
            // answers with the poll watcher.
            Err(error) => {
                fallback.budget.release_native();
                NewWatch {
                    outcome: Err(error),
                    hit_watch_limit: false,
                }
            }
        }
    }

    fn watch_fallback(
        &self,
        fallback: &NativeFallback,
        path: &SanitizedPath,
    ) -> anyhow::Result<WatchOutcome> {
        self.watch_with_fallback_poller(fallback, path.as_path())?;
        Ok(WatchOutcome::Fallback)
    }

    #[cfg(test)]
    fn dispatch(&self, event: notify::Result<notify::Event>) {
        dispatch(self.kind, &self.state, event);
    }

    #[cfg(test)]
    fn dispatch_batch(
        &self,
        first: notify::Result<notify::Event>,
        event_rx: &async_channel::Receiver<notify::Result<notify::Event>>,
    ) {
        dispatch_batch(
            self.kind,
            &self.state,
            &mut diagnostics::RescanHistory::default(),
            first,
            event_rx,
        );
    }

    fn start_native_watch_limit_cooldown(&self, path: &Path) {
        let mut state = self.state.lock();
        let now = Instant::now();
        let should_log = !state.is_native_watch_limit_cooldown_active();
        state.cooldown_until = Some(now + *NATIVE_WATCH_LIMIT_COOLDOWN);
        if should_log {
            log::warn!(
                "OS file watch limit reached while watching {path:?}; skipping new native file watcher registrations for {} seconds",
                NATIVE_WATCH_LIMIT_COOLDOWN.as_secs()
            );
        }
    }

    pub fn remove(&self, id: WatcherRegistrationId) {
        let Some(key) = self
            .state
            .lock()
            .watchers
            .get(&id)
            .map(|registration| registration.key.clone())
        else {
            return;
        };
        let _key_guard = self.lock_key(&key);
        let Some(removed) = self.state.lock().remove_registration(id) else {
            return;
        };
        match &self.fallback {
            Some(fallback) if removed.polled_by_fallback => {
                self.unwatch_with_fallback_poller(fallback, removed.watched_path.as_path());
            }
            _ => {
                self.unwatch(removed.watched_path.as_path()).log_err();
                if let Some(fallback) = &self.fallback {
                    fallback.budget.release_native();
                }
            }
        }
    }

    fn watch(&self, path: &Path) -> anyhow::Result<()> {
        self.ensure_backend()?;
        let result = self
            .backend
            .lock()
            .as_mut()
            .expect("watcher backend initialized")
            .watch(
                path,
                if self.recursive {
                    notify::RecursiveMode::Recursive
                } else {
                    notify::RecursiveMode::NonRecursive
                },
            );
        self.diagnostics.record(|| {
            WatchDiagnosticEvent::new(
                self.kind,
                if result.is_ok() {
                    "watch"
                } else {
                    "watch_error"
                },
                &[path.to_owned()],
                match &result {
                    Ok(()) => format!("recursive={}", self.recursive),
                    Err(error) => format!("{error:?}"),
                },
                false,
            )
        });
        result?;
        Ok(())
    }

    fn unwatch(&self, path: &Path) -> anyhow::Result<()> {
        let watcher = self
            .backend
            .lock()
            .as_mut()
            .map(|watcher| watcher.unwatch(path));

        self.diagnostics.record(|| {
            WatchDiagnosticEvent::new(
                self.kind,
                if matches!(&watcher, Some(Err(_))) {
                    "unwatch_error"
                } else {
                    "unwatch"
                },
                &[path.to_owned()],
                match &watcher {
                    Some(Err(error)) => format!("{error:?}"),
                    _ => String::new(),
                },
                false,
            )
        });
        match watcher {
            // inotify auto-removes a watch when its directory is deleted, so a
            // later unwatch races that and fails with a benign error. Either way
            // the path is no longer watched, which is all we wanted.
            Some(Err(error)) if !matches!(error.kind, notify::ErrorKind::WatchNotFound) => {
                Err(error.into())
            }
            _ => Ok(()),
        }
    }

    /// Takes the poller's baseline of `path` right away, so a change made right after is still
    /// reported.
    fn watch_with_fallback_poller(
        &self,
        fallback: &NativeFallback,
        path: &Path,
    ) -> anyhow::Result<()> {
        let result = fallback.poller.watch(path);
        self.diagnostics.record(|| {
            WatchDiagnosticEvent::new(
                OsWatcherKind::Fallback,
                if result.is_ok() {
                    "watch"
                } else {
                    "watch_error"
                },
                &[path.to_owned()],
                match &result {
                    Ok(()) => "recursive=false".to_owned(),
                    Err(error) => format!("{error:?}"),
                },
                false,
            )
        });
        result?;
        Ok(())
    }

    fn unwatch_with_fallback_poller(&self, fallback: &NativeFallback, path: &Path) {
        let result = fallback.poller.unwatch(path);
        self.diagnostics.record(|| {
            WatchDiagnosticEvent::new(
                OsWatcherKind::Fallback,
                if result.is_ok() {
                    "unwatch"
                } else {
                    "unwatch_error"
                },
                &[path.to_owned()],
                match &result {
                    Ok(()) => String::new(),
                    Err(error) => format!("{error:?}"),
                },
                false,
            )
        });
        result.log_err();
    }

    pub(crate) fn ensure_backend(&self) -> anyhow::Result<()> {
        // The lock is held across creation: with a check-then-insert under two
        // separate lock acquisitions, concurrent callers could each create a
        // watcher and the loser's insert would silently drop the winner's
        // watcher along with every path registered on it.
        let mut backend = self.backend.lock();
        if backend.is_none() {
            *backend = Some(match self.kind {
                OsWatcherKind::Native => {
                    // CORE excludes Access events, which Zed discards anyway. Without this,
                    // the default mask subscribes to inotify OPEN/CLOSE_* on Linux, so every
                    // file read in a watched directory would queue events, increasing the
                    // risk of queue overflows (and thus full rescans) under read-heavy
                    // workloads like grep or language server indexing.
                    let config =
                        notify::Config::default().with_event_kinds(notify::EventKindMask::CORE);
                    Box::new(
                        <notify::RecommendedWatcher as notify::Watcher>::new(
                            self.event_sink(),
                            config,
                        )
                        .inspect_err(|error| self.record_backend_error(error))?,
                    )
                }
                OsWatcherKind::Poll => {
                    let config = notify::Config::default().with_poll_interval(*POLL_INTERVAL);
                    // On Windows the poller serves the 9P share of a WSL distribution, where
                    // every link the distribution made is one Windows cannot follow: following
                    // links would make each pass report an error for each of them. The link
                    // itself is still compared, and neither native backend follows links
                    // either.
                    #[cfg(target_os = "windows")]
                    let config = config.with_follow_symlinks(false);
                    Box::new(
                        notify::PollWatcher::new(self.event_sink(), config)
                            .inspect_err(|error| self.record_backend_error(error))?,
                    )
                }
                OsWatcherKind::Fallback => {
                    anyhow::bail!(
                        "the fallback poller has no backend of its own; it is part of the native watcher"
                    )
                }
            });
        }
        Ok(())
    }

    fn record_backend_error(&self, error: &notify::Error) {
        self.diagnostics.record(|| {
            WatchDiagnosticEvent::new(
                self.kind,
                "backend_error",
                &error.paths,
                format!("{error:?}"),
                false,
            )
        });
    }

    fn demote_native_watches_over_learned_limit(&self, fallback: &NativeFallback) {
        let Some(learned_limit) = fallback.budget.learned_limit() else {
            return;
        };
        let excess =
            native_watches_over_learned_limit(fallback.budget.native_watches(), learned_limit);
        if excess > 0 {
            self.demote_native_watches(fallback, excess);
        }
    }

    fn demote_native_watches(&self, fallback: &NativeFallback, max_demotions: usize) -> usize {
        let candidates = self
            .state
            .lock()
            .paths
            .transition_candidates(false, TransitionOrder::DeepestFirst);
        let mut demoted = 0;
        for key in candidates {
            if demoted >= max_demotions {
                break;
            }
            if self.demote_native_watch(fallback, &key) {
                demoted += 1;
            }
        }
        if demoted > 0 {
            self.ensure_background_thread();
        }
        demoted
    }

    fn demote_native_watch(&self, fallback: &NativeFallback, key: &WatchKey) -> bool {
        let _key_guard = self.lock_key(key);
        let Some(watched_path) = self.state.lock().paths.watched_path(key, false) else {
            return false;
        };
        // The poller takes its baseline before the native watch goes away, so that no change
        // can fall between the two.
        if let Err(error) = self.watch_with_fallback_poller(fallback, watched_path.as_path()) {
            log::debug!(
                "keeping the native watch on {:?}: {error:#}",
                watched_path.as_path()
            );
            return false;
        }
        self.state.lock().paths.set_polled_by_fallback(key, true);
        self.unwatch(watched_path.as_path()).log_err();
        fallback.budget.release_native();
        true
    }

    fn promote_fallback_watches(&self, fallback: &NativeFallback, max_promotions: usize) -> usize {
        let candidates = self
            .state
            .lock()
            .paths
            .transition_candidates(true, TransitionOrder::ShallowestFirst);
        let mut promoted = 0;
        let mut refused_path = None;
        for key in candidates {
            if promoted >= max_promotions {
                break;
            }
            // Other Zed processes may be promoting at the same time, so the room is measured
            // again every few promotions.
            if promoted > 0 && promoted % PROMOTIONS_PER_MEASUREMENT == 0 {
                let reading = fallback.budget.measure();
                if reading
                    .limit
                    .is_none_or(|limit| promotion_capacity(reading.usage, limit) == 0)
                {
                    break;
                }
            }
            match self.promote_fallback_watch(fallback, &key) {
                PromotionOutcome::Promoted => promoted += 1,
                PromotionOutcome::Skipped => {}
                PromotionOutcome::BudgetExhausted => break,
                PromotionOutcome::WatchLimitReached(path) => {
                    refused_path = Some(path);
                    break;
                }
            }
        }
        if let Some(refused_path) = refused_path
            && fallback
                .budget
                .learn_limit_from_watch_limit_error(refused_path.as_path())
        {
            self.demote_native_watches_over_learned_limit(fallback);
        }
        promoted
    }

    fn promote_fallback_watch(
        &self,
        fallback: &NativeFallback,
        key: &WatchKey,
    ) -> PromotionOutcome {
        let _key_guard = self.lock_key(key);
        let Some(watched_path) = self.state.lock().paths.watched_path(key, true) else {
            return PromotionOutcome::Skipped;
        };
        if fallback.poller.has_vanished(watched_path.as_path()) {
            return PromotionOutcome::Skipped;
        }
        if !fallback.budget.try_reserve_native() {
            return PromotionOutcome::BudgetExhausted;
        }
        match self.watch(watched_path.as_path()) {
            Ok(()) => {
                self.state.lock().paths.set_polled_by_fallback(key, false);
                // Changes made since the poller's last pass happened before the native watch
                // existed; one last poll reports them.
                fallback.poller.poll_path(watched_path.as_path());
                self.unwatch_with_fallback_poller(fallback, watched_path.as_path());
                PromotionOutcome::Promoted
            }
            Err(error) if is_max_files_watch_error(&error) => {
                fallback.budget.release_native();
                PromotionOutcome::WatchLimitReached(watched_path)
            }
            Err(error) => {
                fallback.budget.release_native();
                log::debug!("keeping {:?} on polling: {error:#}", watched_path.as_path());
                self.state.lock().paths.mark_native_unavailable(key);
                PromotionOutcome::Skipped
            }
        }
    }

    fn run_budget_maintenance(&self, promote: bool) {
        let Some(fallback) = &self.fallback else {
            return;
        };
        if !fallback.budget.is_enabled() {
            return;
        }
        let (native_paths, fallback_paths) = {
            let state = self.state.lock();
            (
                state.paths.count_watched(false),
                state.paths.count_watched(true),
            )
        };
        if native_paths == 0 && fallback_paths == 0 {
            return;
        }

        // Promotions need current numbers; otherwise the estimate is enough until a measurement
        // is due.
        if promote || fallback.budget.measurement_due(Instant::now()) {
            fallback.budget.measure();
        }
        // Trimming to a limit learned from ENOSPC comes first, since that is what makes room
        // below the limit.
        self.demote_native_watches_over_learned_limit(fallback);
        let reading = fallback.budget.reading();
        let Some(limit) = reading.limit else {
            return;
        };
        let native_watches = fallback.budget.native_watches();
        if should_yield_native_watches(reading.usage, limit) {
            if native_watches > 0 {
                if fallback.budget.begin_yielding() {
                    log::warn!(
                        "only {} of this user's {limit} inotify watches are free; moving {native_watches} watched paths to polling so that other programs can keep watching files",
                        limit.saturating_sub(reading.usage)
                    );
                }
                self.demote_native_watches(fallback, usize::MAX);
            }
            return;
        }

        let over_share = native_watches_over_share(reading.usage, limit, native_watches);
        if over_share > 0 {
            log::info!(
                "{} of this user's {limit} inotify watches are in use, well over Zed's share; moving {over_share} watched paths to polling",
                reading.usage
            );
            self.demote_native_watches(fallback, over_share);
            return;
        }

        if promote && fallback_paths > 0 {
            let capacity = promotion_capacity(reading.usage, limit);
            if capacity > 0 {
                let promoted = self.promote_fallback_watches(fallback, capacity);
                if promoted > 0 {
                    log::info!("moved {promoted} polled paths back to inotify watches");
                    fallback.budget.end_episode();
                }
            }
        }
    }

    fn run_scheduled_maintenance(&self, schedule: &mut MaintenanceSchedule) {
        let now = Instant::now();
        if now < schedule.next_tick {
            return;
        }
        schedule.next_tick = now + MAINTENANCE_INTERVAL;
        let promote = now >= schedule.next_promotion;
        if promote {
            schedule.next_promotion = now + PROMOTION_INTERVAL + promotion_jitter();
        }
        self.run_budget_maintenance(promote);
    }

    fn ensure_background_thread(&self) {
        let Some(fallback) = &self.fallback else {
            return;
        };
        if !fallback.spawn_background_thread {
            return;
        }
        let mut background_thread = fallback.background.thread.lock();
        match *background_thread {
            BackgroundThread::Running => return,
            BackgroundThread::FailedAt(failed_at)
                if failed_at.elapsed() < BACKGROUND_THREAD_RETRY_INTERVAL =>
            {
                return;
            }
            BackgroundThread::NotStarted | BackgroundThread::FailedAt(_) => {}
        }
        let watcher = fallback.this.clone();
        let background = fallback.background.clone();
        match std::thread::Builder::new()
            .name("fs-watcher-fallback".to_owned())
            .spawn(move || run_background_thread(watcher, background))
        {
            Ok(_detached) => *background_thread = BackgroundThread::Running,
            Err(error) => {
                log::error!(
                    "failed to start the fallback file watcher thread, retrying on the next registration; polled paths won't report changes until then: {error}"
                );
                *background_thread = BackgroundThread::FailedAt(Instant::now());
            }
        }
    }
}

impl Drop for OsWatcher {
    fn drop(&mut self) {
        if let Some(fallback) = &self.fallback {
            fallback.background.shut_down();
        }
    }
}

impl NativeFallback {
    fn new(sink: FallbackEventSink, options: FallbackOptions, this: Weak<OsWatcher>) -> Self {
        Self {
            poller: FallbackWatcher::new(sink),
            budget: options.budget,
            background: Arc::new(FallbackBackground::new()),
            spawn_background_thread: options.spawn_background_thread,
            this,
        }
    }
}

/// The fallback poller's events go through the same queue and dispatch as the backend's, and
/// match the same registration table, so a path's callbacks don't care which of the two
/// reported a change.
fn fallback_event_sink(
    event_tx: &async_channel::Sender<notify::Result<notify::Event>>,
    diagnostics: &Arc<diagnostics::DiagnosticRecorder>,
) -> FallbackEventSink {
    let event_tx = event_tx.clone();
    let diagnostics = diagnostics.clone();
    Arc::new(move |event: notify::Event| {
        diagnostics
            .record(|| WatchDiagnosticEvent::from_notify_event(OsWatcherKind::Fallback, &event));
        enqueue(&event_tx, Ok(event));
    })
}

/// Polls the fallback paths and keeps the watch budget up to date for one native watcher.
///
/// The watcher is only held while there is work to do, so one that is dropped between passes
/// goes away without waiting for this thread, which then stops.
fn run_background_thread(watcher: Weak<OsWatcher>, background: Arc<FallbackBackground>) {
    let started = Instant::now();
    let mut schedule = MaintenanceSchedule {
        next_tick: started + MAINTENANCE_INTERVAL,
        next_promotion: started + PROMOTION_INTERVAL,
    };
    let mut next_pass = started + poll_interval();
    loop {
        let wake_at = next_pass.min(schedule.next_tick);
        match background.wait(wake_at) {
            MaintenanceWake::ShutDown => return,
            MaintenanceWake::Requested => schedule.next_tick = Instant::now(),
            MaintenanceWake::Deadline => {}
        }
        let Some(watcher) = watcher.upgrade() else {
            return;
        };
        watcher.run_scheduled_maintenance(&mut schedule);
        if Instant::now() < next_pass {
            continue;
        }
        let Some(fallback) = &watcher.fallback else {
            return;
        };

        let pass_started = Instant::now();
        let mut pacer = PollPacer::new();
        let operation_budget = sweep_operation_budget(poll_interval());
        fallback
            .poller
            .poll_pass(Some(operation_budget), &mut |operations| {
                if let Some(delay) = pacer.record(operations) {
                    std::thread::sleep(delay);
                }
                // A pass over a large tree can take far longer than the maintenance interval.
                if background.take_maintenance_request() {
                    schedule.next_tick = Instant::now();
                }
                watcher.run_scheduled_maintenance(&mut schedule);
            });
        next_pass = (pass_started + poll_interval()).max(Instant::now());
    }
}

fn dispatch(
    kind: OsWatcherKind,
    state: &Mutex<WatcherState>,
    event: notify::Result<notify::Event>,
) {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            log::warn!("{kind:?} watcher error: {error}");
            return;
        }
    };

    log::trace!("handle {kind:?} watcher event: {event:?}");

    let callbacks = {
        let state = state.lock();
        if event.need_rescan() {
            // This also reaches the paths the native watcher handed to its fallback poller,
            // whose native events may be among the lost ones.
            state
                .watchers
                .values()
                .map(|registration| registration.callback.clone())
                .collect::<Vec<_>>()
        } else {
            let mut ids = Vec::new();
            for path in &event.paths {
                let sanitized = SanitizedPath::new(path);
                state.paths.watcher_ids_covering(sanitized, &mut ids);
            }
            ids.sort_unstable_by_key(|id| id.0);
            ids.dedup();
            ids.into_iter()
                .filter_map(|id| state.watchers.get(&id))
                .map(|registration| registration.callback.clone())
                .collect::<Vec<_>>()
        }
    };

    for callback in callbacks {
        callback(&event);
    }
}

fn dispatch_batch(
    kind: OsWatcherKind,
    state: &Mutex<WatcherState>,
    rescan_history: &mut diagnostics::RescanHistory,
    first: notify::Result<notify::Event>,
    event_rx: &async_channel::Receiver<notify::Result<notify::Event>>,
) {
    // A single backend overflow can enqueue many pathless rescan markers, each
    // of which rescans every watched root, so one covers the entire
    // drained batch. Rescans that name paths only cover those subtrees and are
    // all dispatched; ordinary events still run.
    let mut rescan_dispatched = false;

    for event in std::iter::once(first).chain(std::iter::from_fn(|| event_rx.try_recv().ok())) {
        if event
            .as_ref()
            .is_ok_and(|event| event.need_rescan() && event.paths.is_empty())
        {
            if rescan_dispatched {
                continue;
            }
            rescan_dispatched = true;
        }

        // Log writes can block, so keep them off the native watcher reader.
        if let Ok(event) = &event
            && let Some(report) = rescan_history.record(event)
        {
            log::error!("{kind:?} filesystem watcher requested rescan: {report}");
        }
        dispatch(kind, state, event);
    }
}

fn enqueue(
    event_tx: &async_channel::Sender<notify::Result<notify::Event>>,
    event: notify::Result<notify::Event>,
) {
    if matches!(
        event,
        Ok(Event {
            kind: EventKind::Access(_),
            ..
        })
    ) {
        return;
    }

    // A failed send only happens once the dispatch thread has shut down, at
    // which point there's nothing left to dispatch to.
    event_tx.try_send(event).ok();
}

static POLL_INTERVAL: LazyLock<Duration> = LazyLock::new(|| {
    let poll_ms: u64 = std::env::var("ZED_FILE_WATCHER_POLL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
        .clamp(500, 30000);
    Duration::from_millis(poll_ms)
});

static NATIVE_WATCH_LIMIT_COOLDOWN: LazyLock<Duration> = LazyLock::new(|| {
    let cooldown_seconds: u64 = std::env::var("ZED_NATIVE_WATCH_LIMIT_COOLDOWN_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5)
        .clamp(0, 300);
    Duration::from_secs(cooldown_seconds)
});

pub fn poll_interval() -> Duration {
    *POLL_INTERVAL
}

fn is_max_files_watch_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<notify::Error>()
        .is_some_and(|error| matches!(&error.kind, notify::ErrorKind::MaxFilesWatch))
}

/// Whether the volume behind the path has no working native watch, so the path has to be
/// polled instead of being reported as a failure.
///
/// `ReadDirectoryChangesW` answers `ERROR_INVALID_FUNCTION` when the driver serving a
/// directory doesn't implement change notifications. The 9P share of a WSL distribution does
/// that, and so can other network redirectors, which is why this is a rule about the error
/// rather than about the one path spelling `requires_poll_watcher` already knows.
#[cfg(target_os = "windows")]
fn is_unsupported_native_watch_error(error: &anyhow::Error) -> bool {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    error.downcast_ref::<notify::Error>().is_some_and(|error| {
        matches!(&error.kind, notify::ErrorKind::Io(error)
            if error.raw_os_error() == Some(ERROR_INVALID_FUNCTION))
    })
}

/// No other platform answers a watch request with "this volume cannot do that": inotify and
/// FSEvents either work or fail for a reason polling wouldn't fix, and os error 1 means
/// `EPERM` there.
#[cfg(not(target_os = "windows"))]
fn is_unsupported_native_watch_error(_error: &anyhow::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::watch_budget::{InotifyUsage, WatchBudgetSource};
    use super::*;
    use std::{collections::HashSet, fs, path::PathBuf};

    fn rescan(path: &str) -> PathEvent {
        PathEvent {
            path: PathBuf::from(path),
            kind: Some(PathEventKind::Rescan),
        }
    }

    fn changed(path: &str) -> PathEvent {
        PathEvent {
            path: PathBuf::from(path),
            kind: Some(PathEventKind::Changed),
        }
    }

    #[derive(Default)]
    struct FakeWatchBackend {
        watched_paths: HashSet<PathBuf>,
        watch_calls: Vec<PathBuf>,
        unwatch_calls: Vec<PathBuf>,
        fail_with_watch_limit: bool,
        /// Makes `watch` fail with this raw OS error, the way a volume whose driver has no
        /// change notifications does.
        fail_with_io_os_error: Option<i32>,
        max_watches: Option<usize>,
    }

    struct SharedFakeWatchBackend(Arc<Mutex<FakeWatchBackend>>);

    impl WatchBackend for SharedFakeWatchBackend {
        fn watch(&mut self, path: &Path, _mode: notify::RecursiveMode) -> notify::Result<()> {
            let path = path.to_path_buf();
            let mut backend = self.0.lock();
            backend.watch_calls.push(path.clone());
            if let Some(code) = backend.fail_with_io_os_error {
                return Err(notify::Error::io(std::io::Error::from_raw_os_error(code)));
            }
            let at_limit = backend.max_watches.is_some_and(|max_watches| {
                backend.watched_paths.len() >= max_watches && !backend.watched_paths.contains(&path)
            });
            if backend.fail_with_watch_limit || at_limit {
                return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
            }
            backend.watched_paths.insert(path);
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            let path = path.to_path_buf();
            let mut backend = self.0.lock();
            backend.unwatch_calls.push(path.clone());
            if backend.watched_paths.remove(&path) {
                Ok(())
            } else {
                Err(notify::Error::generic("path was not watched"))
            }
        }
    }

    /// Reports the fake native backend's watches plus a configurable number used by other
    /// programs, the way `/proc` reports every inotify watch of the user.
    struct FakeBudgetSource {
        limit: Mutex<Option<usize>>,
        other_programs_usage: Mutex<usize>,
        native_backend: Arc<Mutex<FakeWatchBackend>>,
    }

    impl FakeBudgetSource {
        fn new(limit: Option<usize>, native_backend: Arc<Mutex<FakeWatchBackend>>) -> Arc<Self> {
            Arc::new(Self {
                limit: Mutex::new(limit),
                other_programs_usage: Mutex::new(0),
                native_backend,
            })
        }
    }

    impl WatchBudgetSource for FakeBudgetSource {
        fn limit(&self) -> Option<usize> {
            *self.limit.lock()
        }

        fn usage(&self) -> Option<InotifyUsage> {
            let own_watches = self.native_backend.lock().watched_paths.len();
            Some(InotifyUsage {
                user_watches: *self.other_programs_usage.lock() + own_watches,
                own_watches,
            })
        }
    }

    fn os_watcher_for_test(
        kind: OsWatcherKind,
        backend: Option<Arc<Mutex<FakeWatchBackend>>>,
        fallback: Option<(FallbackEventSink, Arc<WatchBudget>)>,
    ) -> OsWatcher {
        // Tests call `dispatch` directly to exercise dispatch synchronously,
        // rather than going through the OS watcher callbacks, so nothing is ever
        // sent on this channel; the receiver can just be dropped.
        let (event_tx, _event_rx) = async_channel::unbounded();
        OsWatcher {
            kind,
            recursive: kind.is_recursive(),
            state: Arc::new(Mutex::new(WatcherState::new())),
            backend: Mutex::new(
                backend.map(|watcher| {
                    Box::new(SharedFakeWatchBackend(watcher)) as Box<dyn WatchBackend>
                }),
            ),
            event_tx,
            _dispatch_task: Task::ready(()),
            diagnostics: Default::default(),
            key_locks: std::array::from_fn(|_| Mutex::new(())),
            fallback: fallback.map(|(sink, budget)| {
                NativeFallback::new(
                    sink,
                    FallbackOptions {
                        budget,
                        spawn_background_thread: false,
                    },
                    Weak::new(),
                )
            }),
        }
    }

    fn test_os_watcher(
        kind: OsWatcherKind,
        backend: Option<Arc<Mutex<FakeWatchBackend>>>,
    ) -> OsWatcher {
        os_watcher_for_test(kind, backend, None)
    }

    /// A native watcher with a fallback poller whose events are collected rather than
    /// dispatched, so tests can deliver them after running a pass.
    fn test_os_watcher_with_fallback(
        native_backend: Arc<Mutex<FakeWatchBackend>>,
        budget: WatchBudget,
    ) -> (OsWatcher, Arc<Mutex<Vec<notify::Event>>>) {
        let fallback_events = Arc::new(Mutex::new(Vec::new()));
        let fallback_event_sink: FallbackEventSink = {
            let fallback_events = fallback_events.clone();
            Arc::new(move |event| fallback_events.lock().push(event))
        };
        let watcher = os_watcher_for_test(
            OsWatcherKind::Native,
            Some(native_backend),
            Some((fallback_event_sink, Arc::new(budget))),
        );
        (watcher, fallback_events)
    }

    /// A native watcher with the watch budget enabled.
    fn test_os_watcher_with_budget(
        native_backend: Arc<Mutex<FakeWatchBackend>>,
        budget_source: Arc<FakeBudgetSource>,
    ) -> (OsWatcher, Arc<Mutex<Vec<notify::Event>>>) {
        test_os_watcher_with_fallback(native_backend, WatchBudget::new(true, budget_source))
    }

    impl OsWatcher {
        fn fallback_for_test(&self) -> &NativeFallback {
            self.fallback
                .as_ref()
                .expect("this watcher has a fallback poller")
        }
    }

    fn deliver_fallback_events(watcher: &OsWatcher, fallback_events: &Mutex<Vec<notify::Event>>) {
        let events = std::mem::take(&mut *fallback_events.lock());
        for event in events {
            watcher.dispatch(Ok(event));
        }
    }

    fn make_directories(root: &Path, names: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
        names
            .into_iter()
            .map(|name| {
                let directory = root.join(name);
                fs::create_dir_all(&directory).expect("create directory");
                directory
            })
            .collect()
    }

    /// Each directory is one level deeper than the previous one, and none lies inside another,
    /// so recursive native watches (macOS and Windows) never cover one with another.
    fn directories_of_increasing_depth(root: &Path, count: usize) -> Vec<PathBuf> {
        (1..=count)
            .map(|depth| {
                let directory = (0..depth).fold(root.to_path_buf(), |directory, _| {
                    directory.join(format!("branch-{depth}"))
                });
                fs::create_dir_all(&directory).expect("create directory");
                directory
            })
            .collect()
    }

    /// Who watches the registration's path now: the backend (`Native`) or the fallback poller.
    fn registration_kind(watcher: &OsWatcher, id: WatcherRegistrationId) -> Option<OsWatcherKind> {
        let state = watcher.state.lock();
        let registration = state.watchers.get(&id)?;
        let path = state.paths.entries.get(&registration.key)?;
        Some(if path.polled_by_fallback {
            OsWatcherKind::Fallback
        } else {
            watcher.kind
        })
    }

    /// How many paths the backend and the fallback poller each watch.
    fn watched_path_counts(watcher: &OsWatcher) -> (usize, usize) {
        let state = watcher.state.lock();
        (
            state.paths.count_watched(false),
            state.paths.count_watched(true),
        )
    }

    fn path_recorder() -> (
        Arc<Mutex<Vec<PathBuf>>>,
        impl Fn(&notify::Event) + Send + Sync + Clone + 'static,
    ) {
        let fired = Arc::new(Mutex::new(Vec::new()));
        let callback = {
            let fired = fired.clone();
            move |event: &notify::Event| fired.lock().extend(event.paths.iter().cloned())
        };
        (fired, callback)
    }

    /// A registration and who watched its path right after it was made.
    struct TestRegistration {
        id: WatcherRegistrationId,
        kind: Option<OsWatcherKind>,
    }

    fn watch_all(
        watcher: &OsWatcher,
        directories: &[PathBuf],
        callback: impl Fn(&notify::Event) + Send + Sync + Clone + 'static,
    ) -> Vec<TestRegistration> {
        directories
            .iter()
            .map(|directory| {
                let id = watcher
                    .add(directory.as_path().into(), false, callback.clone())
                    .expect("add watch")
                    .expect("watch registered");
                TestRegistration {
                    id,
                    kind: registration_kind(watcher, id),
                }
            })
            .collect()
    }

    fn native_watched_paths(backend: &Mutex<FakeWatchBackend>) -> HashSet<PathBuf> {
        backend.lock().watched_paths.clone()
    }

    struct TestCase {
        name: &'static str,
        pending_paths: Vec<PathEvent>,
        path_events: Vec<PathEvent>,
        expected_pending_paths: Vec<PathEvent>,
        expected_path_events: Vec<PathEvent>,
    }

    #[test]
    fn covered_child_registration_is_not_unwatched_after_parent_is_removed() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_os_watcher(OsWatcherKind::Poll, Some(backend.clone()));
        let parent = Arc::<Path>::from(Path::new("/repo"));
        let child = Arc::<Path>::from(Path::new("/repo/foo.csproj"));

        let parent_registration = watcher
            .add(parent.as_ref().into(), false, |_| {})
            .expect("add parent watch")
            .expect("parent watch registered");
        let child_registration = watcher
            .add(child.as_ref().into(), false, |_| {})
            .expect("add covered child watch")
            .expect("child watch registered");

        watcher.remove(parent_registration);
        watcher.remove(child_registration);

        let backend = backend.lock();
        assert_eq!(backend.watch_calls, &[parent.to_path_buf()]);
        assert_eq!(backend.unwatch_calls, &[parent.to_path_buf()]);
    }

    #[gpui::test]
    async fn pending_path_is_registered_once_created(cx: &mut gpui::TestAppContext) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let path = temp_dir.path().join("file.txt");

        let (tx, rx) = async_channel::unbounded();
        let pending_path_events: Arc<Mutex<Vec<PathEvent>>> = Default::default();
        let watcher = FsWatcher::new(
            OsWatcher::new(OsWatcherKind::Native, cx.executor()),
            OsWatcher::new(OsWatcherKind::Poll, cx.executor()),
            crate::RealFs::new(None, cx.executor()),
            cx.executor(),
            tx,
            pending_path_events.clone(),
        );

        watcher
            .add(&path)
            .expect("add path that does not exist yet");
        assert!(
            watcher
                .pending_registrations
                .lock()
                .contains_key(path.as_path())
        );
        assert!(watcher.registrations.lock().is_empty());

        std::fs::write(&path, b"contents").expect("create path");

        // poll_path_until_created stats the path on smol's blocking pool, which
        // the deterministic executor cannot drive; park until the poll task
        // signals the event channel.
        cx.executor().allow_parking();
        cx.executor().advance_clock(poll_interval());
        rx.recv().await.expect("receive watcher event");

        assert!(
            !watcher
                .pending_registrations
                .lock()
                .contains_key(path.as_path())
        );
        let case_insensitive = case_insensitive_path(&path);
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);
        assert!(watcher.registrations.lock().contains_key(&key));

        // poll_path_until_created also enqueues a Rescan for the same path, but
        // enqueue_path_events -> util::extend_sorted dedups by path, so only Created survives.
        assert_eq!(
            pending_path_events.lock().clone(),
            vec![PathEvent {
                path: path.clone(),
                kind: Some(PathEventKind::Created),
            }]
        );
    }

    #[test]
    fn native_watch_limit_cools_down_subsequent_native_registrations() {
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(native_backend.clone()));
        let first_path = Arc::<Path>::from(Path::new("/repo/first"));
        let second_path = Arc::<Path>::from(Path::new("/repo/second"));

        let first_registration = watcher
            .add(first_path.clone(), false, |_| {})
            .expect("native watch limit is handled");
        let second_registration = watcher
            .add(second_path, false, |_| {})
            .expect("native watch limit backoff is handled");

        assert!(first_registration.is_none());
        assert!(second_registration.is_none());

        let native_backend = native_backend.lock();
        assert_eq!(native_backend.watch_calls, &[first_path.to_path_buf()]);
    }

    /// With a fallback poller the watch limit is never answered with "skipped, retry later",
    /// even when there is no budget to keep within (macOS and Windows).
    #[test]
    fn native_watch_limit_without_a_budget_falls_back_to_polling() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories =
            make_directories(temp_dir.path(), ["first".to_owned(), "second".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let (watcher, _fallback_events) = test_os_watcher_with_fallback(
            native_backend.clone(),
            WatchBudget::new(false, Arc::new(ProcfsBudgetSource)),
        );

        let registrations = watch_all(&watcher, &directories, |_| {});

        // Both paths are watched by polling instead of being skipped and retried later.
        assert!(
            registrations
                .iter()
                .all(|registration| registration.kind == Some(OsWatcherKind::Fallback))
        );
        assert_eq!(native_backend.lock().watch_calls, directories);
    }

    /// A path the fallback poller holds covers nothing below it, even on the platforms where
    /// the native watch it stands in for would have: `FsWatcher` must keep registering the
    /// directories below it.
    #[test]
    fn a_polled_path_does_not_cover_its_descendants() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let parent = temp_dir.path().join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).expect("create directories");
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let (watcher, _fallback_events) = test_os_watcher_with_fallback(
            native_backend.clone(),
            WatchBudget::new(false, Arc::new(ProcfsBudgetSource)),
        );

        let parent_registration = watcher
            .add(parent.as_path().into(), false, |_| {})
            .expect("add parent")
            .expect("parent registered");
        assert!(!watcher.covers_descendants(parent_registration));
        watcher
            .add(child.as_path().into(), false, |_| {})
            .expect("add child")
            .expect("child registered");

        assert_eq!(native_backend.lock().watch_calls, vec![parent, child]);
    }

    /// `\\wsl.localhost\…` is decided on the spelling, before any watch is attempted, and the
    /// `ZED_FILE_WATCHER_MODE` override still wins in both directions.
    #[cfg(target_os = "windows")]
    #[test]
    fn wsl_share_paths_are_polled_unless_the_mode_overrides_it() {
        let shares = [
            Path::new(r"\\wsl.localhost\Ubuntu\home\user\project"),
            Path::new(r"\\wsl$\Ubuntu\home\user\project"),
            Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\home\user\project"),
        ];
        let drive = Path::new(r"I:\zed-jp-llm\crates");

        for path in shares {
            assert!(requires_poll_watcher_with_mode(path, "auto"), "{path:?}");
            assert!(requires_poll_watcher_with_mode(path, "poll"), "{path:?}");
            assert!(!requires_poll_watcher_with_mode(path, "native"), "{path:?}");
        }

        assert!(!requires_poll_watcher_with_mode(drive, "auto"));
        assert!(requires_poll_watcher_with_mode(drive, "poll"));
        assert!(!requires_poll_watcher_with_mode(drive, "native"));
    }

    #[cfg(target_os = "windows")]
    fn drive_leads_to_wsl_share(drive: char, resolve: impl Fn(char) -> Option<String>) -> bool {
        drive_destination(drive, resolve) == Destination::WslShare
    }

    /// A drive letter assigned to a WSL share is the same 9P share, so it has to be polled
    /// too — and, in `worktree`, watched apart from the scan. Nothing in the spelling says
    /// so, so the assignment behind the letter decides.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_drive_letter_assigned_to_a_wsl_share_leads_to_one() {
        // `Z:` alone is assigned, so an assignment that names another letter leads nowhere.
        let assigned =
            |target: &'static str| move |drive: char| (drive == 'Z').then(|| target.to_owned());

        for target in [
            r"\\wsl.localhost\Ubuntu",
            r"\\wsl$\Ubuntu\home\user",
            r"\??\UNC\wsl.localhost\Ubuntu\home",
            r"\Device\LanmanRedirector\;Z:0000000000012345\wsl.localhost\Ubuntu",
            r"\Device\Mup\wsl.localhost\Ubuntu\home\user",
            r"\Device\P9\;Z:000000000005b1d3\wsl$\Ubuntu",
        ] {
            assert!(
                drive_leads_to_wsl_share('Z', assigned(target)),
                "{target:?}"
            );
        }

        for target in [
            r"\\fileserver\share",
            r"\Device\HarddiskVolume3",
            r"\Device\LanmanRedirector\;Z:0000000000012345\fileserver\share",
            r"\??\C:\somewhere",
            r"\Device\Mup\wsl.localhost",
            "",
        ] {
            assert!(
                !drive_leads_to_wsl_share('Z', assigned(target)),
                "{target:?}"
            );
        }

        assert!(!drive_leads_to_wsl_share('Z', |_| None));

        // 実際の Windows への問い合わせも通しておく。この機械の一時ディレクトリのドライブは
        // WSL の共有ではないので、答えは false で、割り当ての無いドライブでも止まらない。
        let temporary = tempfile::tempdir().expect("a temporary directory");
        assert!(!path_is_on_wsl_share(temporary.path()));
        assert!(!path_is_on_wsl_share(Path::new(r"Z:\no-such-assignment")));

        // 上の false だけでは、問い合わせ自体が黙って失敗していても通ってしまう (それは
        // ドライブ文字を見ていなかった頃と同じ答え) ので、実在する文字には答えが出ることを
        // 確かめる。
        if let Some(drive) = drive_letter_of(temporary.path()) {
            let assignment = resolve_drive_assignment(drive)
                .expect("Windows answers what an existing drive letter is assigned to");
            assert!(assignment.starts_with('\\'), "{assignment:?}");
        }
    }

    /// `subst Z: Y:\foo` points a letter at a place below another letter, and `Y:` can be
    /// assigned in turn. The chain is followed to whatever it ends on, carrying the part below
    /// each letter along, and a chain that loops or that runs too deep has to stop.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_chain_of_drive_assignments_is_followed_to_where_it_ends() {
        fn chain(assignments: &'static [(char, &'static str)]) -> impl Fn(char) -> Option<String> {
            move |drive: char| {
                assignments
                    .iter()
                    .find(|(letter, _)| *letter == drive)
                    .map(|(_, assignment)| (*assignment).to_owned())
            }
        }

        // `subst Z: Y:\foo` の Y: が WSL の共有 (`net use` でも、`subst` でも)。
        let through_one_letter: &[(char, &str)] =
            &[('Z', r"\??\Y:\foo"), ('Y', r"\\wsl.localhost\Ubuntu")];
        assert!(drive_leads_to_wsl_share('Z', chain(through_one_letter)));
        assert_eq!(
            unc_spelling_of_drive('Z', chain(through_one_letter)),
            Some(r"\\wsl.localhost\Ubuntu\foo".to_owned())
        );

        // `subst Z: Y:\` は `\??\Y:` と答える (文字の下には何も無い)。
        let onto_the_root_of_the_letter: &[(char, &str)] =
            &[('Z', r"\??\Y:"), ('Y', r"\??\UNC\wsl.localhost\Ubuntu")];
        assert_eq!(
            unc_spelling_of_drive('Z', chain(onto_the_root_of_the_letter)),
            Some(r"\\wsl.localhost\Ubuntu".to_owned())
        );

        let through_two_letters: &[(char, &str)] = &[
            ('Z', r"\??\Y:\foo"),
            ('Y', r"\??\X:\bar"),
            ('X', r"\??\UNC\wsl.localhost\Ubuntu\home"),
        ];
        assert_eq!(
            unc_spelling_of_drive('Z', chain(through_two_letters)),
            Some(r"\\wsl.localhost\Ubuntu\home\bar\foo".to_owned())
        );

        // 文字の下の部分が共有の名前を作ることもある: `\\wsl.localhost` だけでは共有ではない。
        let named_by_what_is_below: &[(char, &str)] =
            &[('Z', r"\??\Y:\Ubuntu"), ('Y', r"\\wsl.localhost")];
        assert!(drive_leads_to_wsl_share('Z', chain(named_by_what_is_below)));
        assert!(!drive_leads_to_wsl_share(
            'Y',
            chain(named_by_what_is_below)
        ));

        for ending_elsewhere in [
            &[('Z', r"\??\Y:\foo"), ('Y', r"\Device\HarddiskVolume3")] as &[(char, &str)],
            &[('Z', r"\??\Y:\foo"), ('Y', r"\\fileserver\share")],
            // 割り当ての無い文字で終わる。
            &[('Z', r"\??\Y:\foo")],
            // `Y:foo` は Y: 自身のカレントディレクトリからの相対で、追える場所ではない。
            &[('Z', r"\??\Y:foo"), ('Y', r"\\wsl.localhost\Ubuntu")],
            // 自分自身に戻る / 2 つの文字で回る。
            &[('Z', r"\??\Z:\a")],
            &[('Z', r"\??\Y:\a"), ('Y', r"\??\Z:\b")],
        ] {
            assert!(
                !drive_leads_to_wsl_share('Z', chain(ending_elsewhere)),
                "{ending_elsewhere:?}"
            );
        }

        // 深さの上限ちょうど (B: から数えて 8 つ) までは届き、その 1 段先 (A:) は届かない。
        let nine_letters: &[(char, &str)] = &[
            ('A', r"\??\B:\1"),
            ('B', r"\??\C:\2"),
            ('C', r"\??\D:\3"),
            ('D', r"\??\E:\4"),
            ('E', r"\??\F:\5"),
            ('F', r"\??\G:\6"),
            ('G', r"\??\H:\7"),
            ('H', r"\??\I:\8"),
            ('I', r"\\wsl.localhost\Ubuntu"),
        ];
        assert!(drive_leads_to_wsl_share('B', chain(nine_letters)));
        assert!(!drive_leads_to_wsl_share('A', chain(nine_letters)));
    }

    /// A worktree opened through a junction or a directory symbolic link onto the share
    /// (`C:\wsl`) is on the share, which only opening the root answers — and a root whose own
    /// spelling already answers is not opened at all.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_root_reached_through_a_reparse_point_is_decided_by_where_it_lands() {
        fn lands_on(answer: &'static str) -> impl Fn(&Path) -> std::io::Result<PathBuf> {
            move |_path: &Path| Ok(PathBuf::from(answer))
        }

        fn cannot_be_opened(_path: &Path) -> std::io::Result<PathBuf> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        }

        let no_drive_is_a_share = |_drive: char| Destination::LocalVolume;
        let root = Path::new(r"C:\wsl");

        for answer in [
            r"\\?\UNC\wsl.localhost\Ubuntu\home\user",
            r"\\?\UNC\wsl$\Ubuntu\home",
            r"\\wsl.localhost\Ubuntu\home",
        ] {
            assert!(
                root_leads_to_wsl_share(root, lands_on(answer), no_drive_is_a_share),
                "{answer:?}"
            );
        }

        for answer in [r"\\?\C:\wsl", r"\\?\UNC\fileserver\share\wsl"] {
            assert!(
                !root_leads_to_wsl_share(root, lands_on(answer), no_drive_is_a_share),
                "{answer:?}"
            );
        }

        // `canonicalize` が `subst` を解いてドライブ文字で答えたら、その文字の割り当てが決める。
        let only_z_is_a_share = |drive: char| {
            if drive == 'Z' {
                Destination::WslShare
            } else {
                Destination::LocalVolume
            }
        };
        assert!(root_leads_to_wsl_share(
            root,
            lands_on(r"\\?\Z:\foo"),
            only_z_is_a_share
        ));

        // 開けない根は綴りだけが答え (まだ存在しない根でも止まらない)。
        assert!(!root_leads_to_wsl_share(
            root,
            cannot_be_opened,
            no_drive_is_a_share
        ));
        assert!(root_leads_to_wsl_share(
            Path::new(r"\\wsl.localhost\Ubuntu\home"),
            cannot_be_opened,
            no_drive_is_a_share
        ));

        // 綴りだけで決まる根は開かない: 共有の上では 9P の往復になる。
        let opens = std::cell::Cell::new(0);
        let counting = |path: &Path| {
            opens.set(opens.get() + 1);
            Ok(PathBuf::from(path))
        };
        assert!(root_leads_to_wsl_share(
            Path::new(r"\\wsl.localhost\Ubuntu\home"),
            &counting,
            no_drive_is_a_share
        ));
        assert_eq!(opens.get(), 0);

        // 他のサーバーの共有も同じ: そこに至る根は、この機械のジャンクションやディレクトリ
        // シンボリックリンクでは無いので、開かずに偽と答える (切れた割り当てを開くと、その
        // サーバーの時間切れまで返らない)。
        for spelling in [
            r"\\fileserver\share\project",
            r"\\?\UNC\fileserver\share\project",
            "//fileserver/share/project",
        ] {
            assert!(
                !root_leads_to_wsl_share(Path::new(spelling), &counting, no_drive_is_a_share),
                "{spelling:?}"
            );
        }
        let drive_is_another_server = |_drive: char| Destination::OtherShare;
        assert!(!root_leads_to_wsl_share(
            Path::new(r"N:\project"),
            &counting,
            drive_is_another_server
        ));
        assert_eq!(opens.get(), 0);

        // ローカルのボリュームの上の根だけが開かれる。
        assert!(!root_leads_to_wsl_share(
            root,
            &counting,
            no_drive_is_a_share
        ));
        assert_eq!(opens.get(), 1);
    }

    /// `\\?\C:\…` と `\\.\…` も 2 つの区切りで始まるが、共有ではない。
    #[cfg(target_os = "windows")]
    #[test]
    fn only_a_server_and_a_share_name_a_share() {
        for share in [
            r"\\fileserver\share",
            r"\\wsl.localhost\Ubuntu\home",
            r"\\?\UNC\fileserver\share",
            "//fileserver/share",
        ] {
            assert!(path_names_a_share(Path::new(share)), "{share:?}");
        }
        for elsewhere in [
            r"C:\project",
            r"\\?\C:\project",
            r"\\.\PhysicalDrive0",
            r"\\?\UNC\",
            r"\\",
            "project",
        ] {
            assert!(!path_names_a_share(Path::new(elsewhere)), "{elsewhere:?}");
        }
    }

    /// Only roots are resolved, so a path answers from the nearest root above it — at a
    /// component boundary, and without telling the spellings of the same place apart.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_path_answers_from_the_nearest_root_above_it() {
        let roots = [
            (watch_root_key(Path::new(r"C:\wsl")), true),
            (watch_root_key(Path::new(r"C:\wsl\windows-side")), false),
            (watch_root_key(Path::new(r"I:\zed-jp-llm")), false),
        ];
        let answer =
            |path: &str| answer_of_nearest_root(&watch_root_key(Path::new(path)), &roots[..]);

        assert_eq!(answer(r"C:\wsl"), Some(true));
        assert_eq!(answer(r"C:\wsl\"), Some(true));
        assert_eq!(answer(r"C:\wsl\home\user\project"), Some(true));
        assert_eq!(answer(r"C:\WSL\Home"), Some(true));
        assert_eq!(answer("C:/wsl/home"), Some(true));
        assert_eq!(answer(r"\\?\C:\wsl\home"), Some(true));
        // より近い根が勝つ。
        assert_eq!(answer(r"C:\wsl\windows-side\src"), Some(false));
        // 前置一致は成分の境目でだけ。
        assert_eq!(answer(r"C:\wslfoo"), None);
        assert_eq!(answer(r"C:\"), None);
        assert_eq!(answer(r"D:\elsewhere"), None);
        assert_eq!(answer_of_nearest_root("c:\\wsl", &[]), None);

        // 判定の入口がその答えを読んでいること。共有に解決した根の代わりに、この試験だけの
        // 綴りを 1 つ置く (本物の綴りは管理者権限が要るディレクトリシンボリックリンク)。
        let root = Path::new(r"C:\zed-fs-watcher-test-root-that-leads-to-a-wsl-share");
        WSL_SHARE_ROOTS.lock().push((watch_root_key(root), true));
        assert!(path_is_on_wsl_share(root));
        assert!(path_is_on_wsl_share(&root.join("home").join("user")));
        assert!(!path_is_on_wsl_share(Path::new(
            r"C:\zed-fs-watcher-test-root-that-leads-to-a-wsl-share-elsewhere"
        )));
    }

    /// The production wiring: `note_watch_root` records a real root under the very key the
    /// paths below it are looked up with, once — announcing the same place again, however it
    /// is spelled, does not record it a second time.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_root_is_recorded_once_under_the_key_its_paths_are_looked_up_with() {
        let temporary = tempfile::tempdir().expect("a temporary directory");
        let root = temporary.path();
        let key = watch_root_key(root);

        let recorded = || {
            WSL_SHARE_ROOTS
                .lock()
                .iter()
                .filter(|(known, _)| *known == key)
                .map(|(_, is_share)| *is_share)
                .collect::<Vec<bool>>()
        };
        assert_eq!(recorded(), Vec::<bool>::new());

        note_watch_root(root);
        // 一時ディレクトリはこの機械のボリュームの上なので、答えは「共有ではない」。
        assert_eq!(recorded(), vec![false]);

        let spelled = root.to_string_lossy().into_owned();
        note_watch_root(root);
        note_watch_root(Path::new(&spelled.to_uppercase()));
        note_watch_root(Path::new(&format!("{spelled}\\")));
        note_watch_root(Path::new(&spelled.replace('\\', "/")));
        assert_eq!(recorded(), vec![false]);
    }

    /// A `subst` assignment made for the duration of a test, removed however the test ends.
    #[cfg(target_os = "windows")]
    struct SubstDrive {
        letter: char,
    }

    #[cfg(target_os = "windows")]
    impl SubstDrive {
        fn create(target: &Path) -> Option<Self> {
            let letter = unused_drive_letter()?;
            let created = smol::block_on(
                smol::process::Command::new("subst")
                    .arg(format!("{letter}:"))
                    .arg(target)
                    .status(),
            )
            .expect("running subst");
            created.success().then_some(Self { letter })
        }
    }

    #[cfg(target_os = "windows")]
    impl Drop for SubstDrive {
        fn drop(&mut self) {
            // 消し損ねるとドライブ文字が次の再起動まで残るので、失敗は黙って捨てない。
            // panic 中にも走るので、ここでは panic しない。
            match smol::block_on(
                smol::process::Command::new("subst")
                    .arg(format!("{}:", self.letter))
                    .arg("/D")
                    .status(),
            ) {
                Ok(status) if status.success() => {}
                Ok(status) => eprintln!("subst {}: /D exited with {status}", self.letter),
                Err(error) => eprintln!("subst {}: /D could not run: {error}", self.letter),
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn unused_drive_letter() -> Option<char> {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;

        let in_use = unsafe { GetLogicalDrives() };
        // 後ろから: 前の方の文字は取り外し可能な機器に割り当てられやすい。
        ('D'..='Z')
            .rev()
            .find(|letter| in_use & (1 << (*letter as u32 - 'A' as u32)) == 0)
    }

    /// The spellings the chain is read from are the ones Windows really answers with: a
    /// `subst` on another `subst`, and a junction that `canonicalize` sees through. The
    /// assignments and the link this makes are removed again.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_spells_a_real_chain_the_way_it_is_read() {
        let temporary = tempfile::tempdir().expect("a temporary directory");
        let below = temporary.path().join("below");
        std::fs::create_dir(&below).expect("a directory below the temporary one");

        let Some(first) = SubstDrive::create(temporary.path()) else {
            eprintln!("no unused drive letter, so the chain could not be made");
            return;
        };
        let Some(second) = SubstDrive::create(Path::new(&format!(r"{}:\below", first.letter)))
        else {
            eprintln!("no second unused drive letter, so the chain could not be made");
            return;
        };

        let assignment = resolve_drive_assignment(second.letter)
            .expect("Windows answers what a subst drive is assigned to");
        assert_eq!(assignment, format!(r"\??\{}:\below", first.letter));

        // 連鎖の先はこの機械のローカルボリュームなので、共有ではない。
        assert!(!drive_leads_to_wsl_share(
            second.letter,
            resolve_drive_assignment
        ));

        // 本当に追っていることは、途中の文字だけを共有に差し替えると答えが変わることで分かる。
        let first_letter = first.letter;
        let as_if_the_first_were_a_share = |drive: char| {
            if drive == first_letter {
                Some(r"\\wsl.localhost\Ubuntu".to_owned())
            } else {
                resolve_drive_assignment(drive)
            }
        };
        assert_eq!(
            unc_spelling_of_drive(second.letter, &as_if_the_first_were_a_share),
            Some(r"\\wsl.localhost\Ubuntu\below".to_owned())
        );
        assert!(drive_leads_to_wsl_share(
            second.letter,
            as_if_the_first_were_a_share
        ));

        // ジャンクションを `subst` したドライブの上に作ると、根の綴りのドライブ文字と、
        // `canonicalize` が答えるドライブ文字が別になる = 根を開いた答えを読んでいることが
        // 分かる。(UNC を指すリンクは `mklink /D` = 管理者権限が要るので、ここでは作れない。)
        let junction = PathBuf::from(format!(r"{}:\junction", first.letter));
        let created = smol::block_on(
            smol::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&junction)
                .arg(&below)
                .output(),
        )
        .expect("running mklink");
        assert!(
            created.status.success(),
            "mklink /J: {}",
            String::from_utf8_lossy(&created.stderr)
        );

        let landed = std::fs::canonicalize(&junction).expect("the junction can be opened");
        assert_eq!(
            landed,
            std::fs::canonicalize(&below).expect("the directory below can be opened")
        );
        let landed_drive =
            drive_letter_of(&landed).expect("canonicalize answers with a drive letter");
        assert_ne!(landed_drive, first.letter);

        assert!(!root_leads_to_wsl_share(
            &junction,
            |path: &Path| std::fs::canonicalize(path),
            |_drive| Destination::LocalVolume
        ));
        assert!(root_leads_to_wsl_share(
            &junction,
            |path: &Path| std::fs::canonicalize(path),
            move |drive| if drive == landed_drive {
                Destination::WslShare
            } else {
                Destination::LocalVolume
            }
        ));
    }

    /// The whole of it against this machine's WSL: a `subst` on a `subst` on the share is
    /// polled, although nothing in the letter's spelling says where it leads.
    ///
    /// It assigns two drive letters, so it runs only when asked for:
    ///
    /// ```pwsh
    /// $env:ZED_E2E_WSL_SUBST=1
    /// cargo test -p fs --lib -- --ignored --nocapture e2e_wsl_share_reached
    /// ```
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn e2e_wsl_share_reached_through_two_drive_letters_is_polled() {
        if !std::env::var("ZED_E2E_WSL_SUBST").is_ok_and(|value| value == "1") {
            eprintln!("set ZED_E2E_WSL_SUBST=1 to run this against this machine's WSL");
            return;
        }
        let distro = std::env::var("ZED_E2E_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".to_owned());
        let share = PathBuf::from(format!(r"\\wsl.localhost\{distro}\home"));
        assert!(share.exists(), "{} has to be reachable", share.display());

        let first = SubstDrive::create(&share).expect("an unused drive letter for the share");
        let second = SubstDrive::create(Path::new(&format!(r"{}:\", first.letter)))
            .expect("a second unused drive letter");

        assert!(drive_leads_to_wsl_share(
            first.letter,
            resolve_drive_assignment
        ));
        assert!(drive_leads_to_wsl_share(
            second.letter,
            resolve_drive_assignment
        ));
        assert!(requires_poll_watcher(Path::new(&format!(
            r"{}:\",
            second.letter
        ))));
    }

    /// Only a path that starts with a drive letter has an assignment to resolve, and a path
    /// relative to a drive's own current directory is not one of them.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_path_names_the_drive_whose_assignment_is_resolved() {
        assert_eq!(drive_letter_of(Path::new(r"z:\project\src")), Some('Z'));
        assert_eq!(drive_letter_of(Path::new("Z:/project/src")), Some('Z'));
        assert_eq!(drive_letter_of(Path::new(r"\\?\Z:\project")), Some('Z'));
        assert_eq!(drive_letter_of(Path::new("Z:")), Some('Z'));

        assert_eq!(drive_letter_of(Path::new(r"\\wsl.localhost\Ubuntu")), None);
        assert_eq!(drive_letter_of(Path::new(r"Z:project\src")), None);
        assert_eq!(drive_letter_of(Path::new(r"\project\src")), None);
        assert_eq!(drive_letter_of(Path::new("project")), None);
    }

    /// A volume that isn't a WSL share but still has no `ReadDirectoryChangesW` is caught by
    /// the failure of the native watch itself, and polled by the poll watcher instead.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    fn a_native_watch_the_volume_cannot_serve_falls_back_to_polling(cx: &mut gpui::TestAppContext) {
        const ERROR_INVALID_FUNCTION: i32 = 1;
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["share".to_owned()]);
        let directory = directories.first().expect("one directory").clone();
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_io_os_error: Some(ERROR_INVALID_FUNCTION),
            ..Default::default()
        }));
        let poll_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let native_watcher = Arc::new(test_os_watcher(
            OsWatcherKind::Native,
            Some(native_backend.clone()),
        ));
        let poll_watcher = Arc::new(test_os_watcher(
            OsWatcherKind::Poll,
            Some(poll_backend.clone()),
        ));
        let fs = crate::RealFs::new(None, cx.executor());
        let (tx, _rx) = async_channel::unbounded();

        let registration = register_existing_path(
            &native_watcher,
            &poll_watcher,
            fs.as_ref(),
            directory.as_path().into(),
            false,
            tx,
            Default::default(),
        )
        .expect("the path is polled instead")
        .expect("the path is registered");

        assert!(Arc::ptr_eq(&registration.os_watcher, &poll_watcher));
        assert!(registration.recursive);
        assert_eq!(native_watched_paths(&native_backend), HashSet::new());
        assert_eq!(
            native_watched_paths(&poll_backend),
            HashSet::from_iter([directory])
        );
    }

    /// Only "this volume cannot do that" is answered with polling: an error polling wouldn't
    /// fix still reaches the caller.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    fn another_native_watch_error_is_reported_rather_than_polled(cx: &mut gpui::TestAppContext) {
        const ERROR_ACCESS_DENIED: i32 = 5;
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["denied".to_owned()]);
        let directory = directories.first().expect("one directory").clone();
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_io_os_error: Some(ERROR_ACCESS_DENIED),
            ..Default::default()
        }));
        let poll_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let native_watcher = Arc::new(test_os_watcher(OsWatcherKind::Native, Some(native_backend)));
        let poll_watcher = Arc::new(test_os_watcher(
            OsWatcherKind::Poll,
            Some(poll_backend.clone()),
        ));
        let fs = crate::RealFs::new(None, cx.executor());
        let (tx, _rx) = async_channel::unbounded();

        let result = register_existing_path(
            &native_watcher,
            &poll_watcher,
            fs.as_ref(),
            directory.as_path().into(),
            false,
            tx,
            Default::default(),
        );

        assert!(result.is_err());
        assert!(poll_backend.lock().watch_calls.is_empty());
    }

    /// The 9P share of a WSL distribution, end to end: a directory created inside WSL is
    /// watched through `\\wsl.localhost\…`, a file is created inside WSL, and the event has to
    /// reach the watcher — which it does only by polling, because `ReadDirectoryChangesW`
    /// answers `ERROR_INVALID_FUNCTION` for that share.
    ///
    /// It needs this machine's WSL and writes in the distribution's `/tmp`, so it runs only
    /// when asked for:
    ///
    /// ```text
    /// ZED_E2E_WSL_WATCH=1 cargo test -p fs --lib -- --ignored --nocapture e2e_wsl
    /// ```
    #[cfg(target_os = "windows")]
    #[gpui::test]
    #[ignore]
    async fn e2e_wsl_share_changes_reach_the_watcher(cx: &mut gpui::TestAppContext) {
        use futures::{FutureExt as _, StreamExt as _};

        if !std::env::var("ZED_E2E_WSL_WATCH").is_ok_and(|value| value == "1") {
            return;
        }
        let distro = std::env::var("ZED_E2E_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".to_owned());
        let run = |script: String| {
            let output = smol::block_on(
                smol::process::Command::new("wsl.exe")
                    .args(["-d", distro.as_str(), "--exec", "sh", "-c", script.as_str()])
                    .output(),
            )
            .expect("running wsl.exe");
            assert!(
                output.status.success(),
                "{script}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run("rm -rf /tmp/zed-4a-watch; mkdir -p /tmp/zed-4a-watch; \
             echo hi > /tmp/zed-4a-watch/target.txt; \
             ln -s target.txt /tmp/zed-4a-watch/link.txt"
            .to_owned());

        let share = PathBuf::from(format!(r"\\wsl.localhost\{distro}\tmp\zed-4a-watch"));
        assert!(
            requires_poll_watcher(&share),
            "a distribution's own filesystem is polled"
        );

        // What `worktree`'s scan tells a link WSL made apart by: the link itself can be read,
        // but anything that follows it answers `ERROR_PATH_NOT_FOUND` — including the open
        // behind `Fs::metadata`, which is why such an entry is skipped rather than reported.
        const ERROR_PATH_NOT_FOUND: i32 = 3;
        let link = share.join("link.txt");
        assert!(
            fs::symlink_metadata(&link)
                .expect("the link itself is readable")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::File::open(&link)
                .err()
                .and_then(|error| error.raw_os_error()),
            Some(ERROR_PATH_NOT_FOUND),
            "Windows cannot follow a link the distribution made"
        );
        assert_eq!(
            fs::metadata(&link)
                .err()
                .and_then(|error| error.raw_os_error()),
            Some(ERROR_PATH_NOT_FOUND)
        );
        // 辿れないリンクは、リンク自身を開き直して file id を取る道が残っている (壊れた
        // symlink を普通のドライブで走査するため)。その開き直しが 9P の共有でも通るなら
        // リンクは worktree に載り、通らなければ `worktree` はこのエラーで見分けて飛ばす。
        if let Err(file_id_error) = crate::file_id(&link).await {
            assert_eq!(
                file_id_error
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error),
                Some(ERROR_PATH_NOT_FOUND),
                "this is the error `worktree` recognizes such a link by"
            );
        }

        // The poller reports from its own thread by the wall clock, which the test executor
        // doesn't drive, so this waits for real events on a real timer.
        cx.executor().allow_parking();
        let fs = crate::RealFs::new(None, cx.executor());
        let (mut events, watcher) = fs.watch(&share, Duration::ZERO).await;

        run("touch /tmp/zed-4a-watch/from-wsl.txt".to_owned());

        // Long enough for several poll passes over 9P, which is slower than a local volume.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut reported: Vec<PathEvent> = Vec::new();
        while !reported
            .iter()
            .any(|event| event.path.ends_with("from-wsl.txt"))
            && Instant::now() < deadline
        {
            futures::select_biased! {
                batch = events.next().fuse() => match batch {
                    Some(batch) => reported.extend(batch),
                    None => break,
                },
                _ = cx.executor().timer(Duration::from_millis(250)).fuse() => {}
            }
        }

        drop(events);
        drop(watcher);
        run("rm -rf /tmp/zed-4a-watch".to_owned());

        assert!(
            reported
                .iter()
                .any(|event| event.path.ends_with("from-wsl.txt")),
            "the file created inside WSL was reported: {reported:?}"
        );
    }

    #[test]
    fn native_watches_beyond_the_budget_fall_back_and_still_deliver_events() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(
            temp_dir.path(),
            (0..20).map(|index| format!("dir-{index:02}")),
        );
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(40), native_backend.clone());
        let (watcher, fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);

        // A quarter of the limit of 40 is 10 watches for the whole user.
        let native_registrations = registrations
            .iter()
            .filter(|registration| registration.kind == Some(OsWatcherKind::Native))
            .count();
        assert_eq!(native_registrations, 10);
        assert_eq!(native_backend.lock().watched_paths.len(), 10);
        assert_eq!(watcher.fallback_for_test().budget.native_watches(), 10);

        let fallback_directory = registrations
            .iter()
            .zip(&directories)
            .find(|(registration, _)| registration.kind == Some(OsWatcherKind::Fallback))
            .map(|(_, directory)| directory)
            .expect("paths beyond the budget are polled");
        let file = fallback_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*fired.lock(), vec![file]);
    }

    #[test]
    fn watch_limit_error_falls_back_and_is_never_retried() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories =
            make_directories(temp_dir.path(), (0..3).map(|index| format!("dir-{index}")));
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let budget_source = FakeBudgetSource::new(Some(1_000_000), native_backend.clone());
        let (watcher, fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);
        for _ in 0..5 {
            watcher.run_budget_maintenance(true);
        }

        assert!(
            registrations
                .iter()
                .all(|registration| registration.kind == Some(OsWatcherKind::Fallback))
        );
        assert_eq!(native_backend.lock().watch_calls, &directories[..1]);
        assert_eq!(watcher.fallback_for_test().budget.learned_limit(), Some(0));

        let file = directories[2].join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*fired.lock(), vec![file]);
    }

    #[test]
    fn watch_limit_error_teaches_the_effective_limit_and_demotes_the_excess() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(
            temp_dir.path(),
            (0..60).map(|index| format!("dir-{index:02}")),
        );
        // `/proc` claims plenty of room, but the kernel refuses the 41st watch, as it does
        // inside a user namespace with a lowered limit.
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            max_watches: Some(40),
            ..Default::default()
        }));
        let budget_source = FakeBudgetSource::new(Some(1_000_000), native_backend.clone());
        let (watcher, fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);
        // Trimming to the learned limit is left to the maintenance thread.
        watcher.run_budget_maintenance(false);

        let budget = &watcher.fallback_for_test().budget;
        assert_eq!(budget.learned_limit(), Some(40));
        assert_eq!(native_backend.lock().watched_paths.len(), 10);
        assert_eq!(budget.native_watches(), 10);
        assert_eq!(native_backend.lock().watch_calls.len(), 41);
        assert_eq!(watched_path_counts(&watcher), (10, 50));
        assert_eq!(watcher.state.lock().watchers.len(), 60);

        let demoted_directory = registrations
            .iter()
            .zip(&directories)
            .find(|(registration, _)| {
                registration.kind == Some(OsWatcherKind::Native)
                    && registration_kind(&watcher, registration.id) == Some(OsWatcherKind::Fallback)
            })
            .map(|(_, directory)| directory)
            .expect("some native watches were handed to the poller");
        let file = demoted_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*fired.lock(), vec![file]);
    }

    #[test]
    fn demotion_and_promotion_keep_the_registration_and_deliver_changes_in_between() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directory = temp_dir.path().join("watched");
        fs::create_dir(&directory).expect("create watched dir");
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source.clone());
        let (fired, callback) = path_recorder();

        let registration = watcher
            .add(directory.as_path().into(), false, callback)
            .expect("add watch")
            .expect("watch registered");
        assert_eq!(
            registration_kind(&watcher, registration),
            Some(OsWatcherKind::Native)
        );

        // Other programs leave less than an eighth of the limit free.
        *budget_source.other_programs_usage.lock() = 990;
        watcher.fallback_for_test().budget.measure();
        watcher.run_budget_maintenance(false);
        assert!(native_backend.lock().watched_paths.is_empty());
        assert_eq!(
            registration_kind(&watcher, registration),
            Some(OsWatcherKind::Fallback)
        );
        assert_eq!(watcher.fallback_for_test().budget.native_watches(), 0);

        let polled_file = directory.join("while-polled.txt");
        write_settled_file(&polled_file);
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(std::mem::take(&mut *fired.lock()), vec![polled_file]);

        // This change happens after the last pass, right before the native watch takes over.
        let handover_file = directory.join("during-handover.txt");
        write_settled_file(&handover_file);
        *budget_source.other_programs_usage.lock() = 0;
        watcher.run_budget_maintenance(true);
        assert_eq!(
            native_watched_paths(&native_backend),
            HashSet::from_iter([directory.clone()])
        );
        assert_eq!(
            registration_kind(&watcher, registration),
            Some(OsWatcherKind::Native)
        );
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(std::mem::take(&mut *fired.lock()), vec![handover_file]);

        // The poller no longer reports the path once it is watched natively again.
        write_settled_file(&directory.join("after-promotion.txt"));
        watcher.fallback_for_test().poller.poll_once();
        assert!(fallback_events.lock().is_empty());

        watcher.remove(registration);
        assert!(native_backend.lock().watched_paths.is_empty());
        assert_eq!(watcher.fallback_for_test().budget.native_watches(), 0);
        assert!(watcher.state.lock().watchers.is_empty());
    }

    /// The fallback poller's paths and events show up in the debug window's recording, apart
    /// from the backend's.
    #[test]
    fn diagnostics_show_the_paths_and_events_of_the_fallback_poller() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories =
            make_directories(temp_dir.path(), ["native".to_owned(), "polled".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            max_watches: Some(1),
            ..Default::default()
        }));
        let mut watcher = os_watcher_for_test(OsWatcherKind::Native, Some(native_backend), None);
        watcher.fallback = Some(NativeFallback::new(
            fallback_event_sink(&watcher.event_tx, &watcher.diagnostics),
            FallbackOptions {
                budget: Arc::new(WatchBudget::new(false, Arc::new(ProcfsBudgetSource))),
                spawn_background_thread: false,
            },
            Weak::new(),
        ));
        let watcher = Arc::new(watcher);
        let recording = WatchRecording::new([watcher.clone()]);

        watch_all(&watcher, &directories, |_| {});
        let file = directories[1].join("created.txt");
        write_settled_file(&file);
        watcher.fallback_for_test().poller.poll_once();

        let snapshot = recording.snapshot();
        let roots_of = |backend: OsWatcherKind| {
            snapshot
                .watchers
                .iter()
                .filter(|watcher| watcher.backend == backend)
                .flat_map(|watcher| watcher.roots.iter().map(|root| PathBuf::from(&root.path)))
                .collect::<Vec<_>>()
        };
        assert_eq!(roots_of(OsWatcherKind::Native), &directories[..1]);
        assert_eq!(roots_of(OsWatcherKind::Fallback), &directories[1..]);
        assert!(snapshot.events.iter().any(|event| {
            event.backend == OsWatcherKind::Fallback
                && event.operation == "watch"
                && event.paths == [directories[1].to_string_lossy().into_owned()]
        }));
        assert!(snapshot.events.iter().any(|event| {
            event.backend == OsWatcherKind::Fallback
                && event.operation == "event"
                && event.paths == [file.to_string_lossy().into_owned()]
        }));
    }

    #[test]
    fn demotion_starts_with_the_deepest_paths() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = directories_of_increasing_depth(temp_dir.path(), 5);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            max_watches: Some(4),
            ..Default::default()
        }));
        let budget_source = FakeBudgetSource::new(Some(1_000_000), native_backend.clone());
        let (watcher, _fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source);

        watch_all(&watcher, &directories, |_| {});
        watcher.run_budget_maintenance(false);

        // The fifth watch hits the limit of 4, which leaves room for a single native watch.
        assert_eq!(
            native_watched_paths(&native_backend),
            HashSet::from_iter([directories[0].clone()])
        );
    }

    #[test]
    fn promotion_starts_with_the_shallowest_paths() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = directories_of_increasing_depth(temp_dir.path(), 5);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(12), native_backend.clone());
        *budget_source.other_programs_usage.lock() = 100;
        let (watcher, _fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source.clone());

        let registrations = watch_all(&watcher, &directories, |_| {});
        assert!(
            registrations
                .iter()
                .all(|registration| registration.kind == Some(OsWatcherKind::Fallback))
        );

        // A quarter of 12 is 3 watches.
        *budget_source.other_programs_usage.lock() = 0;
        watcher.run_budget_maintenance(true);
        assert_eq!(
            native_watched_paths(&native_backend),
            HashSet::from_iter(directories[..3].iter().cloned())
        );
    }

    #[test]
    fn registrations_of_a_polled_path_join_it() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["watched".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        // A quarter of 3 rounds down to no native watches at all.
        let budget_source = FakeBudgetSource::new(Some(3), native_backend.clone());
        let (watcher, fallback_events) = test_os_watcher_with_budget(native_backend, budget_source);
        let (first_fired, first_callback) = path_recorder();
        let (second_fired, second_callback) = path_recorder();

        let first = watch_all(&watcher, &directories, first_callback);
        let second = watch_all(&watcher, &directories, second_callback);
        assert_eq!(first[0].kind, Some(OsWatcherKind::Fallback));
        assert_eq!(second[0].kind, Some(OsWatcherKind::Fallback));

        let file = directories[0].join("created.txt");
        write_settled_file(&file);
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*first_fired.lock(), vec![file.clone()]);
        assert_eq!(*second_fired.lock(), vec![file]);

        // The poll only stops once the last registration is gone.
        watcher.remove(first[0].id);
        write_settled_file(&directories[0].join("second.txt"));
        watcher.fallback_for_test().poller.poll_once();
        assert_eq!(fallback_events.lock().len(), 1);
        fallback_events.lock().clear();

        watcher.remove(second[0].id);
        write_settled_file(&directories[0].join("third.txt"));
        watcher.fallback_for_test().poller.poll_once();
        assert!(fallback_events.lock().is_empty());
    }

    /// Hands every native watch over to the poller, as when other programs are about to run out
    /// of inotify watches, and then frees the watches again.
    fn yield_native_watches(watcher: &OsWatcher, budget_source: &FakeBudgetSource) {
        let limit = budget_source.limit.lock().unwrap_or_default();
        *budget_source.other_programs_usage.lock() = limit;
        watcher.fallback_for_test().budget.measure();
        watcher.run_budget_maintenance(false);
        *budget_source.other_programs_usage.lock() = 0;
    }

    fn watched_directory(root: &Path) -> PathBuf {
        let directory = root.join("watched");
        fs::create_dir(&directory).expect("create watched dir");
        directory
    }

    /// Writes a file whose mtime lies well in the past, so the poller doesn't treat it as
    /// racily clean and report it once more on its next pass.
    fn write_settled_file(path: &Path) {
        fs::write(path, b"x").expect("write file");
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open file")
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .expect("set mtime");
    }

    #[test]
    fn native_events_queued_before_a_demotion_are_still_delivered() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directory = watched_directory(temp_dir.path());
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, _fallback_events) =
            test_os_watcher_with_budget(native_backend, budget_source.clone());
        let (fired, callback) = path_recorder();
        let registration = watcher
            .add(directory.as_path().into(), false, callback)
            .expect("add watch")
            .expect("watch registered");

        yield_native_watches(&watcher, &budget_source);
        assert_eq!(
            registration_kind(&watcher, registration),
            Some(OsWatcherKind::Fallback)
        );

        // The native watcher reported this before the hand-over; dispatch runs later.
        let file = directory.join("queued.txt");
        watcher.dispatch(Ok(notify::Event::new(EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(file.clone())));
        assert_eq!(*fired.lock(), vec![file]);
    }

    #[test]
    fn native_rescan_reaches_paths_handed_to_the_poller() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["a".to_owned(), "b".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, _fallback_events) =
            test_os_watcher_with_budget(native_backend, budget_source.clone());
        let (first_fired, first_callback) = fired_count();
        let (second_fired, second_callback) = fired_count();
        watch_all(&watcher, &directories[..1], first_callback);
        watch_all(&watcher, &directories[1..], second_callback);

        yield_native_watches(&watcher, &budget_source);
        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        watcher.dispatch(Ok(rescan));

        assert_eq!(*first_fired.lock(), 1);
        assert_eq!(*second_fired.lock(), 1);
    }

    #[test]
    fn removing_a_polled_registration_leaves_the_native_count_alone() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(
            temp_dir.path(),
            (0..11).map(|index| format!("dir-{index:02}")),
        );
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(40), native_backend.clone());
        let (watcher, _fallback_events) =
            test_os_watcher_with_budget(native_backend, budget_source);
        let registrations = watch_all(&watcher, &directories, |_| {});
        let budget = &watcher.fallback_for_test().budget;
        assert_eq!(budget.native_watches(), 10);

        let polled = registrations
            .iter()
            .find(|registration| registration.kind == Some(OsWatcherKind::Fallback))
            .expect("the eleventh path is polled");
        watcher.remove(polled.id);
        assert_eq!(budget.native_watches(), 10);

        let native = registrations
            .iter()
            .find(|registration| registration.kind == Some(OsWatcherKind::Native))
            .expect("the first paths are watched natively");
        watcher.remove(native.id);
        assert_eq!(budget.native_watches(), 9);
    }

    #[test]
    fn every_registration_of_a_path_survives_demotion_and_promotion() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directory = watched_directory(temp_dir.path());
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, fallback_events) =
            test_os_watcher_with_budget(native_backend.clone(), budget_source.clone());
        let (first_fired, first_callback) = path_recorder();
        let (second_fired, second_callback) = path_recorder();
        let first = watcher
            .add(directory.as_path().into(), false, first_callback)
            .expect("add first watch")
            .expect("first watch registered");
        let second = watcher
            .add(directory.as_path().into(), false, second_callback)
            .expect("add second watch")
            .expect("second watch registered");
        assert_eq!(native_backend.lock().watch_calls.len(), 1);

        yield_native_watches(&watcher, &budget_source);
        for id in [first, second] {
            assert_eq!(
                registration_kind(&watcher, id),
                Some(OsWatcherKind::Fallback)
            );
        }
        let polled_file = directory.join("polled.txt");
        write_settled_file(&polled_file);
        watcher.fallback_for_test().poller.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(
            std::mem::take(&mut *first_fired.lock()),
            vec![polled_file.clone()]
        );
        assert_eq!(std::mem::take(&mut *second_fired.lock()), vec![polled_file]);

        let handover_file = directory.join("handover.txt");
        write_settled_file(&handover_file);
        watcher.run_budget_maintenance(true);
        for id in [first, second] {
            assert_eq!(registration_kind(&watcher, id), Some(OsWatcherKind::Native));
        }
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*first_fired.lock(), vec![handover_file.clone()]);
        assert_eq!(*second_fired.lock(), vec![handover_file]);
    }

    fn modify_event(path: &str) -> notify::Event {
        notify::Event {
            paths: vec![PathBuf::from(path)],
            ..notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
        }
    }

    fn recording_watcher() -> (OsWatcher, Arc<Mutex<Vec<String>>>) {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(backend));
        let fired = Arc::new(Mutex::new(Vec::new()));
        for dir in ["/repo/a", "/repo/a/nested", "/repo/b"] {
            let fired = fired.clone();
            let label = dir.to_owned();
            watcher
                .add(Arc::<Path>::from(Path::new(dir)), false, move |_| {
                    fired.lock().push(label.clone());
                })
                .expect("add watch")
                .expect("watch registered");
        }
        (watcher, fired)
    }

    #[test]
    fn event_dispatches_only_to_registrations_covering_its_path() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(Ok(modify_event("/repo/a/file.txt")));

        // Only the directory containing the file resolves; siblings stay untouched.
        assert_eq!(*fired.lock(), vec!["/repo/a".to_owned()]);
    }

    #[test]
    fn event_dispatches_to_every_ancestor_registration() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(Ok(modify_event("/repo/a/nested/file.txt")));

        // Both the directory containing the file and the ancestor watching it
        // resync, each exactly once, matching the previous broadcast-and-filter
        // behavior.
        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(got, vec!["/repo/a".to_owned(), "/repo/a/nested".to_owned()]);
    }

    fn fired_count() -> (
        Arc<Mutex<usize>>,
        impl Fn(&notify::Event) + Send + Sync + Clone + 'static,
    ) {
        let fired = Arc::new(Mutex::new(0usize));
        let cb = {
            let fired = fired.clone();
            move |_: &notify::Event| *fired.lock() += 1
        };
        (fired, cb)
    }

    #[test]
    fn watch_key_folds_case_but_keeps_exact_distinct() {
        let mixed = SanitizedPath::new(Path::new("/Repo/Proj"));
        let lower = SanitizedPath::new(Path::new("/repo/proj"));

        // Folded keys collide regardless of casing; exact keys do not.
        assert_eq!(WatchKey::folded(mixed), WatchKey::folded(lower));
        assert_ne!(WatchKey::exact(mixed), WatchKey::exact(lower));
        // Exact and folded live in different key spaces even for the same path.
        assert_ne!(WatchKey::exact(mixed), WatchKey::folded(mixed));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn watch_key_folds_unicode_normalization_on_macos() {
        // "Café" precomposed (NFC) vs decomposed (NFD) are different byte
        // sequences but the same directory on a normalization-insensitive volume.
        let nfc = Path::new("/repo/Caf\u{00e9}");
        let nfd = Path::new("/repo/Cafe\u{0301}");
        assert_ne!(nfc, nfd);
        assert_eq!(
            WatchKey::folded(SanitizedPath::new(nfc)),
            WatchKey::folded(SanitizedPath::new(nfd)),
        );
    }

    #[test]
    fn case_insensitive_registration_matches_differently_cased_event() {
        let (fired, cb) = fired_count();
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(Default::default()));
        watcher
            .add(Path::new("/Repo/Project").into(), true, cb)
            .expect("add")
            .expect("registered");

        // Event arrives lowercased (as TSGO/macOS may report it).
        watcher.dispatch(Ok(modify_event("/repo/project/file.txt")));
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_insensitive_registration_survives_case_only_rename() {
        let (fired, cb) = fired_count();
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(Default::default()));
        watcher
            .add(Path::new("/Repo/Proj").into(), true, cb)
            .expect("add")
            .expect("registered");

        // The watched directory was renamed to a different casing; events now
        // arrive under the new spelling.
        watcher.dispatch(Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_sensitive_registration_ignores_differently_cased_event() {
        let (fired, cb) = fired_count();
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(Default::default()));
        watcher
            .add(Path::new("/Repo/proj").into(), false, cb)
            .expect("add")
            .expect("registered");

        // On a case-sensitive volume these are genuinely different directories.
        watcher.dispatch(Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 0);
    }

    #[test]
    fn differently_cased_adds_dedupe_on_case_insensitive_volume() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(backend.clone()));
        watcher
            .add(Path::new("/Repo/Proj").into(), true, |_| {})
            .expect("add")
            .expect("registered");
        watcher
            .add(Path::new("/repo/proj").into(), true, |_| {})
            .expect("add")
            .expect("registered");

        // The second, differently-cased spelling reuses the same OS watch.
        assert_eq!(backend.lock().watch_calls.len(), 1);
    }

    #[test]
    fn recursive_parent_covers_differently_cased_child() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_os_watcher(OsWatcherKind::Poll, Some(backend.clone()));
        watcher
            .add(Path::new("/Repo").into(), true, |_| {})
            .expect("add")
            .expect("registered");
        watcher
            .add(Path::new("/repo/child").into(), true, |_| {})
            .expect("add");

        // The child is covered by the recursive parent despite the case mismatch.
        assert_eq!(backend.lock().watch_calls, vec![PathBuf::from("/Repo")]);
    }

    #[test]
    fn rescan_event_broadcasts_to_all_registrations_of_the_same_backend() {
        let (watcher, fired) = recording_watcher();

        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        watcher.dispatch(Ok(rescan));

        // A pathless rescan may have missed events anywhere, so every registration
        // of that backend resyncs, regardless of which directory it watches.
        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                "/repo/a".to_owned(),
                "/repo/a/nested".to_owned(),
                "/repo/b".to_owned(),
            ]
        );
    }

    #[test]
    fn queued_rescans_are_coalesced_without_dropping_normal_events() {
        let (watcher, fired) = recording_watcher();
        let (event_tx, event_rx) = async_channel::unbounded();
        let rescan = || notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);

        event_tx.try_send(Ok(rescan())).unwrap();
        event_tx
            .try_send(Ok(modify_event("/repo/a/file.txt")))
            .unwrap();
        watcher.dispatch_batch(Ok(rescan()), &event_rx);

        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                "/repo/a".to_owned(),
                "/repo/a".to_owned(),
                "/repo/a/nested".to_owned(),
                "/repo/b".to_owned(),
            ]
        );
    }

    #[test]
    fn queued_rescans_naming_distinct_paths_are_all_dispatched() {
        let (watcher, fired) = recording_watcher();
        let (event_tx, event_rx) = async_channel::unbounded();
        let rescan_of = |path: &str| {
            notify::Event::new(EventKind::Other)
                .set_flag(notify::event::Flag::Rescan)
                .add_path(PathBuf::from(path))
        };

        event_tx.try_send(Ok(rescan_of("/repo/b"))).unwrap();
        watcher.dispatch_batch(Ok(rescan_of("/repo/a/nested")), &event_rx);

        // Each named rescan only covers its own subtree once it reaches the
        // callbacks, so collapsing the second one would lose /repo/b's resync.
        // Both are broadcast to every registration of the backend.
        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                "/repo/a".to_owned(),
                "/repo/a".to_owned(),
                "/repo/a/nested".to_owned(),
                "/repo/a/nested".to_owned(),
                "/repo/b".to_owned(),
                "/repo/b".to_owned(),
            ]
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn event_dispatches_when_reported_path_has_verbatim_prefix() {
        // `notify` (and Windows APIs generally) can report a changed path using the
        // verbatim `\\?\` long-path form even when the directory was registered
        // without it.
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_os_watcher(OsWatcherKind::Native, Some(backend));
        let fired = Arc::new(Mutex::new(Vec::new()));
        {
            let fired = fired.clone();
            watcher
                .add(
                    Arc::<Path>::from(Path::new("C:\\repo\\src")),
                    false,
                    move |_| fired.lock().push(()),
                )
                .expect("add watch")
                .expect("watch registered");
        }

        watcher.dispatch(Ok(modify_event("\\\\?\\C:\\repo\\src\\main.rs")));

        assert_eq!(fired.lock().len(), 1);
    }

    #[test]
    fn test_coalesce_pending_rescans() {
        let test_cases = [
            TestCase {
                name: "coalesces descendant rescans under pending ancestor",
                pending_paths: vec![rescan("/root")],
                path_events: vec![rescan("/root/child"), rescan("/root/child/grandchild")],
                expected_pending_paths: vec![rescan("/root")],
                expected_path_events: vec![],
            },
            TestCase {
                name: "new ancestor rescan replaces pending descendant rescans",
                pending_paths: vec![
                    changed("/other"),
                    rescan("/root/child"),
                    rescan("/root/child/grandchild"),
                ],
                path_events: vec![rescan("/root")],
                expected_pending_paths: vec![changed("/other")],
                expected_path_events: vec![rescan("/root")],
            },
            TestCase {
                name: "same path rescan replaces pending non-rescan event",
                pending_paths: vec![changed("/root")],
                path_events: vec![rescan("/root")],
                expected_pending_paths: vec![],
                expected_path_events: vec![rescan("/root")],
            },
            TestCase {
                name: "unrelated rescans are preserved",
                pending_paths: vec![rescan("/root-a")],
                path_events: vec![rescan("/root-b")],
                expected_pending_paths: vec![rescan("/root-a")],
                expected_path_events: vec![rescan("/root-b")],
            },
            TestCase {
                name: "batch ancestor rescan replaces descendant rescan",
                pending_paths: vec![],
                path_events: vec![rescan("/root/child"), rescan("/root")],
                expected_pending_paths: vec![],
                expected_path_events: vec![rescan("/root")],
            },
        ];

        for test_case in test_cases {
            let mut pending_paths = test_case.pending_paths;
            let mut path_events = test_case.path_events;

            coalesce_pending_rescans(&mut pending_paths, &mut path_events);

            assert_eq!(
                pending_paths, test_case.expected_pending_paths,
                "pending_paths mismatch for case: {}",
                test_case.name
            );
            assert_eq!(
                path_events, test_case.expected_path_events,
                "path_events mismatch for case: {}",
                test_case.name
            );
        }
    }

    /// Measures only this process, since other processes of the same user (such as a Zed
    /// connected to the machine running the tests) would make the checked numbers
    /// unpredictable.
    #[cfg(target_os = "linux")]
    struct ProcessOnlyBudgetSource {
        limit: Option<usize>,
    }

    #[cfg(target_os = "linux")]
    impl WatchBudgetSource for ProcessOnlyBudgetSource {
        fn limit(&self) -> Option<usize> {
            self.limit.or_else(watch_budget::read_inotify_watch_limit)
        }

        fn usage(&self) -> Option<InotifyUsage> {
            let own_watches = watch_budget::count_process_inotify_watches(Path::new("/proc/self"));
            Some(InotifyUsage {
                user_watches: own_watches,
                own_watches,
            })
        }
    }

    /// The real-inotify tests count every inotify watch of the test process, so they run one
    /// at a time.
    #[cfg(target_os = "linux")]
    static REAL_INOTIFY_TESTS: Mutex<()> = Mutex::new(());

    /// Shows the watcher's warnings (such as the limit it learned) in the test output.
    #[cfg(target_os = "linux")]
    struct StderrLogger;

    #[cfg(target_os = "linux")]
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!("[{}] {}", record.level(), record.args());
            }
        }

        fn flush(&self) {}
    }

    #[cfg(target_os = "linux")]
    fn install_stderr_logger() {
        static LOGGER: StderrLogger = StderrLogger;
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }

    /// Watches fresh directories through a real `FsWatcher` whose native watcher keeps to a
    /// budget read from `budget_source`, checks that a directory left to the fallback poller
    /// reports a new file, and returns how many inotify watches this process held once every
    /// directory was registered, with the limit the watcher learned from ENOSPC.
    #[cfg(target_os = "linux")]
    fn watch_directories_with_real_inotify(
        cx: &mut gpui::TestAppContext,
        directory_count: usize,
        budget_source: Arc<dyn WatchBudgetSource>,
    ) -> (usize, Option<usize>) {
        install_stderr_logger();
        // The poller reports from its own thread by the wall clock.
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(
            temp_dir.path(),
            (0..directory_count).map(|index| format!("dir-{index:04}")),
        );
        let native_watcher = OsWatcher::build(
            OsWatcherKind::Native,
            cx.executor(),
            None,
            Some(FallbackOptions {
                budget: Arc::new(WatchBudget::new(true, budget_source)),
                spawn_background_thread: true,
            }),
        );
        let (tx, _rx) = async_channel::unbounded();
        let pending_path_events: Arc<Mutex<Vec<PathEvent>>> = Default::default();
        let watcher = FsWatcher::new(
            native_watcher.clone(),
            OsWatcher::new(OsWatcherKind::Poll, cx.executor()),
            crate::RealFs::new(None, cx.executor()),
            cx.executor(),
            tx,
            pending_path_events.clone(),
        );
        for directory in &directories {
            watcher.add(directory).expect("watch directory");
        }
        assert!(
            watcher.pending_registrations.lock().is_empty(),
            "no path should be left retrying"
        );
        let budget = native_watcher.fallback_for_test().budget.clone();
        // Trimming to a learned limit happens on the maintenance thread.
        let deadline = Instant::now() + Duration::from_secs(10);
        while budget
            .learned_limit()
            .is_some_and(|learned_limit| budget.native_watches() > learned_limit / 4)
        {
            assert!(
                Instant::now() < deadline,
                "the maintenance thread never trimmed to the learned limit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let own_watches = watch_budget::count_process_inotify_watches(Path::new("/proc/self"));
        let (native_paths, polled_paths) = watched_path_counts(&native_watcher);
        eprintln!(
            "registered {directory_count} directories: {native_paths} native, {polled_paths} polled; budget counts {} native watches, learned limit {:?}; /proc/self lists {own_watches} inotify watches",
            budget.native_watches(),
            budget.learned_limit(),
        );

        let fallback_directory = directories
            .iter()
            .find(|directory| {
                let key =
                    WatchKey::for_registration(SanitizedPath::new(directory.as_path()), false);
                let id = watcher
                    .registrations
                    .lock()
                    .get(&key)
                    .map(|registration| registration.id);
                id.and_then(|id| registration_kind(&native_watcher, id))
                    == Some(OsWatcherKind::Fallback)
            })
            .expect("some directories are left to the fallback poller");
        let file = fallback_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        // The poller's thread queues the event; dispatching it is the test executor's work.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            cx.run_until_parked();
            if pending_path_events
                .lock()
                .iter()
                .any(|event| event.path == file)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the fallback poller never reported {file:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let learned_limit = budget.learned_limit();
        drop(watcher);
        (own_watches, learned_limit)
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    fn real_inotify_watches_stay_within_an_injected_limit(cx: &mut gpui::TestAppContext) {
        const LIMIT: usize = 400;
        let _serialized = REAL_INOTIFY_TESTS.lock();

        let (own_watches, _) = watch_directories_with_real_inotify(
            cx,
            300,
            Arc::new(ProcessOnlyBudgetSource { limit: Some(LIMIT) }),
        );

        eprintln!("inotify watches held with an injected limit of {LIMIT}: {own_watches}");
        assert!(
            own_watches <= LIMIT / 4,
            "{own_watches} inotify watches exceed a quarter of {LIMIT}"
        );
        assert!(
            own_watches > 0,
            "directories within the budget should still use inotify"
        );
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    #[ignore = "needs a lowered inotify limit that /proc overstates: run inside `unshare -Ur` after writing a small value such as 400 to /proc/sys/user/max_inotify_watches, with ZED_INOTIFY_MAX_USER_WATCHES set above it"]
    fn real_inotify_limit_is_learned_from_watch_limit_errors(cx: &mut gpui::TestAppContext) {
        let _serialized = REAL_INOTIFY_TESTS.lock();

        let (own_watches, learned_limit) = watch_directories_with_real_inotify(
            cx,
            1000,
            Arc::new(ProcessOnlyBudgetSource { limit: None }),
        );

        let learned_limit = learned_limit
            .expect("hitting the lowered limit should teach the watcher the effective limit");
        eprintln!(
            "learned an inotify limit of {learned_limit}; inotify watches held: {own_watches}"
        );
        assert!(
            own_watches <= learned_limit / 4,
            "{own_watches} inotify watches exceed a quarter of the learned limit {learned_limit}"
        );
    }
}
