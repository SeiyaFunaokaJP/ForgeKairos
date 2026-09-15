use gpui::{BackgroundExecutor, Task};
use notify::{Event, EventKind};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    ops::DerefMut,
    path::Path,
    sync::{
        Arc, LazyLock, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use util::{ResultExt, paths::SanitizedPath};

use crate::{PathEvent, PathEventKind, Watcher};

mod fallback_watcher;
mod watch_budget;

use fallback_watcher::{FallbackEventSink, FallbackWatcher, PollPacer, sweep_operation_budget};
use watch_budget::{
    MAINTENANCE_INTERVAL, PROMOTION_INTERVAL, PROMOTIONS_PER_MEASUREMENT, ProcfsBudgetSource,
    WatchBudget, native_watches_over_learned_limit, native_watches_over_share, promotion_capacity,
    should_yield_native_watches,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WatcherMode {
    #[default]
    Native,
    Poll,
    /// Non-recursive polling of a single path, used on Linux in place of an inotify watch when
    /// Zed's watches would crowd out other programs running as the same user.
    Fallback,
}

impl WatcherMode {
    fn is_recursive(self) -> bool {
        match self {
            WatcherMode::Native => cfg!(any(target_os = "windows", target_os = "macos")),
            WatcherMode::Poll => true,
            WatcherMode::Fallback => false,
        }
    }
}

pub struct FsWatcher {
    executor: BackgroundExecutor,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    registrations: Arc<Mutex<HashMap<WatchKey, FsWatcherRegistration>>>,
    pending_registrations: Arc<Mutex<HashMap<Arc<std::path::Path>, Task<()>>>>,
}

#[derive(Clone, Copy)]
struct FsWatcherRegistration {
    id: WatcherRegistrationId,
    /// The mode the path was registered with. It only decides whether the registration covers
    /// descendants, which later hand-overs between the native and fallback watchers don't
    /// change since both watch a single directory on the platforms that hand over.
    mode: WatcherMode,
}

impl FsWatcher {
    pub fn new(
        executor: BackgroundExecutor,
        tx: async_channel::Sender<()>,
        pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    ) -> Self {
        Self {
            executor,
            tx,
            pending_path_events,
            registrations: Default::default(),
            pending_registrations: Default::default(),
        }
    }

    fn add_existing_path(&self, path: Arc<Path>) -> anyhow::Result<()> {
        let case_insensitive = case_insensitive_path(&path);
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);
        if self.registrations.lock().contains_key(&key) {
            log::trace!("path to watch is already watched: {path:?}");
            return Ok(());
        }
        let registration = register_existing_path(
            path,
            case_insensitive,
            self.tx.clone(),
            self.pending_path_events.clone(),
        )?;
        self.registrations.lock().insert(key, registration);
        Ok(())
    }

    fn add_pending_path(&self, path: Arc<Path>) {
        let mut pending_registrations = self.pending_registrations.lock();
        if pending_registrations.contains_key(path.as_ref()) {
            return;
        }

        let task = self.executor.spawn(poll_path_until_created(
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

        let global_watcher = global_watcher();
        for (_, registration) in registrations {
            global_watcher.remove(registration.id);
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

        if fs::symlink_metadata(path.as_ref()).is_err() {
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
            global_watcher().remove(registration.id);
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
                    .is_some_and(|registration| registration.mode.is_recursive())
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
        // (ADR-F49 decision 33). Decided on the spelling alone: this runs for every
        // registration, and touching the share to ask would cost a round trip through 9P.
        return util::path_mapping::is_wsl_share_path(path);
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
        false
    }
}

fn register_existing_path(
    path: Arc<Path>,
    case_insensitive: bool,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
) -> anyhow::Result<FsWatcherRegistration> {
    let mode = if requires_poll_watcher(path.as_ref()) {
        log::info!(
            "Using poll watcher ({}ms interval) for {}",
            poll_interval().as_millis(),
            path.display()
        );
        telemetry::event!("fs_watcher_poll", path = path.display().to_string());
        WatcherMode::Poll
    } else {
        WatcherMode::Native
    };
    let root_path = SanitizedPath::new_arc(path.as_ref());
    let path_for_callback = path.clone();
    global_watcher().add(
        path,
        mode,
        case_insensitive,
        move |event: &notify::Event| {
            log::trace!("watcher received event: {event:?}");
            push_notify_event(
                &tx,
                &pending_path_events,
                &root_path,
                case_insensitive,
                path_for_callback.as_ref(),
                event,
            );
        },
    )
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
fn case_insensitive_path(path: &Path) -> bool {
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
fn case_insensitive_path(_path: &Path) -> bool {
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
fn case_insensitive_path(_path: &Path) -> bool {
    // todo(windows): Windows defaults to case in sensitive, but
    // they can mark specific directories as case sensitive. Mainly
    // for WSL use cases
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn case_insensitive_path(_path: &Path) -> bool {
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

        if smol::fs::symlink_metadata(path.as_ref()).await.is_err() {
            continue;
        }

        // Probe case sensitivity now that the path exists, rather than at add
        // time when it didn't.
        let case_insensitive = case_insensitive_path(path.as_ref());
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);

        if registrations.lock().contains_key(&key) {
            pending_registrations.lock().remove(path.as_ref());
            return;
        }

        match register_existing_path(
            path.clone(),
            case_insensitive,
            tx.clone(),
            pending_path_events.clone(),
        ) {
            Ok(registration) => {
                {
                    let mut pending_registrations = pending_registrations.lock();
                    if pending_registrations.remove(path.as_ref()).is_none() {
                        global_watcher().remove(registration.id);
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
        if !watcher_logging_rate_limited() {
            log::warn!("filesystem watcher lost sync for {watched_root:?}; scheduling rescan");
        }

        path_events.retain(|path_event| path_event.path != watched_root);
        path_events.push(PathEvent {
            path: watched_root.to_path_buf(),
            kind: Some(PathEventKind::Rescan),
        });
    }
    log::trace!("path_events: {:?}", path_events);
    enqueue_path_events(tx, pending_path_events, path_events);
}

fn watcher_logging_rate_limited() -> bool {
    static LAST_WARN: Mutex<Option<(Instant, usize)>> = Mutex::new(None);
    let Some((ref mut started, ref mut emitted)) = *LAST_WARN.lock() else {
        *LAST_WARN.lock() = Some((Instant::now(), 0));
        return false;
    };

    if started.elapsed().as_secs() < 1 {
        if *emitted < 20 {
            log::warn!("filesystem watcher lost sync for many files, not logging more");
            return true;
        } else {
            *emitted += 1;
        }
    } else {
        *emitted = 0;
        *started = Instant::now()
    }

    true
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
    mode: WatcherMode,
}

struct PathRegistrationState {
    watcher_ids: Vec<WatcherRegistrationId>,
    has_os_watcher: bool,
    /// The spelling the backend watch was created with. Unwatching the path, or handing it to
    /// another backend, has to use the same one.
    watched_path: Arc<SanitizedPath>,
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

/// The registered watch paths for one watcher mode, keyed by [`WatchKey`] so that
/// add (dedup), remove, and dispatch share a single notion of path identity.
#[derive(Default)]
struct WatchPaths {
    entries: HashMap<WatchKey, PathRegistrationState>,
    /// Lets dispatch skip building folded keys while no case-insensitive path is registered.
    folded_entries: usize,
}

impl WatchPaths {
    fn contains(&self, key: &WatchKey) -> bool {
        self.entries.contains_key(key)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn has_folded_entries(&self) -> bool {
        self.folded_entries > 0
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

    fn insert(&mut self, key: WatchKey, registration: PathRegistrationState) {
        match self.entries.entry(key) {
            Entry::Occupied(mut occupied) => occupied
                .get_mut()
                .watcher_ids
                .extend(registration.watcher_ids),
            Entry::Vacant(vacant) => {
                if matches!(vacant.key(), WatchKey::Folded(_)) {
                    self.folded_entries += 1;
                }
                vacant.insert(registration);
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

    fn watched_path(&self, key: &WatchKey) -> Option<Arc<SanitizedPath>> {
        self.entries
            .get(key)
            .filter(|registration| registration.has_os_watcher)
            .map(|registration| registration.watched_path.clone())
    }

    fn mark_native_unavailable(&mut self, key: &WatchKey) {
        if let Some(registration) = self.entries.get_mut(key) {
            registration.native_unavailable = true;
        }
    }

    fn transition_candidates(&self, order: TransitionOrder) -> Vec<WatchKey> {
        let mut candidates = self
            .entries
            .iter()
            .filter(|(_, registration)| {
                registration.has_os_watcher && !registration.native_unavailable
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
    /// `path`. Only poll watches and native macOS/Windows watches are recursive.
    fn covered_by_recursive_ancestor(&self, path: &SanitizedPath, mode: WatcherMode) -> bool {
        if !mode.is_recursive() {
            return false;
        }
        path.as_path().ancestors().skip(1).any(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            self.entries.contains_key(&WatchKey::exact(ancestor))
                || self.entries.contains_key(&WatchKey::folded(ancestor))
        })
    }

    /// Collects the watcher ids of the registrations at any of `ancestor_keys`, the keys of an
    /// event path and its ancestors.
    fn watcher_ids_for(
        &self,
        ancestor_keys: &[AncestorKeys],
        ids: &mut Vec<WatcherRegistrationId>,
    ) {
        for ancestor in ancestor_keys {
            if let Some(registration) = self.entries.get(&ancestor.exact) {
                ids.extend_from_slice(&registration.watcher_ids);
            }
            if let Some(folded) = &ancestor.folded
                && let Some(registration) = self.entries.get(folded)
            {
                ids.extend_from_slice(&registration.watcher_ids);
            }
        }
    }
}

struct AncestorKeys {
    exact: WatchKey,
    folded: Option<WatchKey>,
}

/// The keys of `path` and each of its ancestors. Both exact and folded keys are probed, so a
/// real-cased event path matches a folded registration and vice versa; folded keys are only
/// built while a case-insensitive path is registered.
fn ancestor_keys(path: &SanitizedPath, include_folded: bool) -> Vec<AncestorKeys> {
    path.as_path()
        .ancestors()
        .map(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            AncestorKeys {
                exact: WatchKey::exact(ancestor),
                folded: include_folded.then(|| WatchKey::folded(ancestor)),
            }
        })
        .collect()
}

#[derive(Default)]
struct WatcherState {
    watchers: HashMap<WatcherRegistrationId, WatcherRegistrationState>,
    native_path_registrations: WatchPaths,
    poll_path_registrations: WatchPaths,
    fallback_path_registrations: WatchPaths,
    last_registration: WatcherRegistrationId,
}

impl WatcherState {
    fn path_registrations(&self, mode: WatcherMode) -> &WatchPaths {
        match mode {
            WatcherMode::Native => &self.native_path_registrations,
            WatcherMode::Poll => &self.poll_path_registrations,
            WatcherMode::Fallback => &self.fallback_path_registrations,
        }
    }

    fn path_registrations_mut(&mut self, mode: WatcherMode) -> &mut WatchPaths {
        match mode {
            WatcherMode::Native => &mut self.native_path_registrations,
            WatcherMode::Poll => &mut self.poll_path_registrations,
            WatcherMode::Fallback => &mut self.fallback_path_registrations,
        }
    }

    /// A path moves between the native and fallback watchers while events from the one it
    /// left may still be queued, so events from either are matched against both. A key is
    /// only ever registered with one of the two.
    fn dispatch_targets(&self, mode: WatcherMode) -> [Option<&WatchPaths>; 2] {
        match mode {
            WatcherMode::Poll => [Some(&self.poll_path_registrations), None],
            WatcherMode::Native | WatcherMode::Fallback => [
                Some(&self.native_path_registrations),
                Some(&self.fallback_path_registrations),
            ],
        }
    }

    /// The mode a new watcher on `key` can join without a backend call, and whether a new path
    /// entry would own a backend watch: either the path is registered already (with whichever
    /// of the native and fallback watchers holds it), or a recursive ancestor covers it.
    fn joinable_registration(
        &self,
        key: &WatchKey,
        path: &SanitizedPath,
        mode: WatcherMode,
    ) -> Option<(WatcherMode, bool)> {
        let modes_sharing_key: &[WatcherMode] = if mode == WatcherMode::Poll {
            &[WatcherMode::Poll]
        } else {
            &[WatcherMode::Native, WatcherMode::Fallback]
        };
        if let Some(registered_mode) = modes_sharing_key
            .iter()
            .copied()
            .find(|registered_mode| self.path_registrations(*registered_mode).contains(key))
        {
            return Some((registered_mode, true));
        }
        self.path_registrations(mode)
            .covered_by_recursive_ancestor(path, mode)
            .then_some((mode, false))
    }

    fn register_watcher(
        &mut self,
        key: WatchKey,
        path: Arc<SanitizedPath>,
        mode: WatcherMode,
        callback: Arc<dyn Fn(&notify::Event) + Send + Sync>,
        has_os_watcher: bool,
    ) -> WatcherRegistrationId {
        let id = self.last_registration;
        self.last_registration = WatcherRegistrationId(id.0 + 1);
        self.watchers.insert(
            id,
            WatcherRegistrationState {
                callback,
                key: key.clone(),
                mode,
            },
        );
        self.path_registrations_mut(mode)
            .add_watcher(key, id, || PathRegistrationState {
                watcher_ids: vec![id],
                has_os_watcher,
                watched_path: path,
                native_unavailable: false,
            });
        id
    }

    fn move_path_registration(&mut self, key: &WatchKey, from: WatcherMode, to: WatcherMode) {
        let Some(path_state) = self.path_registrations_mut(from).remove(key) else {
            return;
        };
        for id in &path_state.watcher_ids {
            if let Some(registration) = self.watchers.get_mut(id) {
                registration.mode = to;
            }
        }
        self.path_registrations_mut(to)
            .insert(key.clone(), path_state);
    }

    fn remove_registration(
        &mut self,
        id: WatcherRegistrationId,
    ) -> Option<(Arc<SanitizedPath>, WatcherMode)> {
        let registration_state = self.watchers.remove(&id)?;
        let path_registrations = self.path_registrations_mut(registration_state.mode);
        let path_state = path_registrations.get_mut(&registration_state.key)?;
        path_state.watcher_ids.retain(|&existing| existing != id);
        if !path_state.watcher_ids.is_empty() {
            return None;
        }

        let path_state = path_registrations.remove(&registration_state.key)?;
        path_state
            .has_os_watcher
            .then_some((path_state.watched_path, registration_state.mode))
    }
}

trait WatchBackend: Send {
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

type DispatchEvent = (WatcherMode, Result<notify::Event, notify::Error>);

const KEY_LOCK_STRIPES: usize = 64;

pub struct GlobalWatcher {
    state: Mutex<WatcherState>,

    // DANGER: never keep state lock while holding watcher lock
    // two mutexes because calling watcher.add triggers watcher.event, which needs watchers.
    native_watcher: Mutex<Option<Box<dyn WatchBackend>>>,
    poll_watcher: Mutex<Option<Box<dyn WatchBackend>>>,
    fallback_watcher: FallbackWatcher,
    budget: WatchBudget,
    // Adding, removing, and handing a path between the native and fallback watchers each span
    // several state updates with backend calls in between, so these operations are serialized
    // per watch key. The locks are striped so that unrelated paths don't wait on each other's
    // backend calls. A key lock is taken before the state, backend, and budget locks, and no
    // thread ever holds two of them at once.
    key_locks: [Mutex<()>; KEY_LOCK_STRIPES],
    spawn_background_thread: bool,
    background_thread: Mutex<BackgroundThread>,
    maintenance_requested: Mutex<bool>,
    maintenance_wake: Condvar,
    event_tx: async_channel::Sender<DispatchEvent>,
}

enum BackgroundThread {
    NotStarted,
    Running,
    FailedAt(Instant),
}

const BACKGROUND_THREAD_RETRY_INTERVAL: Duration = Duration::from_secs(5);

struct NewWatch {
    mode: anyhow::Result<WatcherMode>,
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

impl GlobalWatcher {
    fn new(
        event_tx: async_channel::Sender<DispatchEvent>,
        native_watcher: Option<Box<dyn WatchBackend>>,
        poll_watcher: Option<Box<dyn WatchBackend>>,
        fallback_event_sink: FallbackEventSink,
        budget: WatchBudget,
        spawn_background_thread: bool,
    ) -> Self {
        Self {
            state: Mutex::new(WatcherState::default()),
            native_watcher: Mutex::new(native_watcher),
            poll_watcher: Mutex::new(poll_watcher),
            fallback_watcher: FallbackWatcher::new(fallback_event_sink),
            budget,
            key_locks: std::array::from_fn(|_| Mutex::new(())),
            spawn_background_thread,
            background_thread: Mutex::new(BackgroundThread::NotStarted),
            maintenance_requested: Mutex::new(false),
            maintenance_wake: Condvar::new(),
            event_tx,
        }
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
        mode: WatcherMode,
        case_insensitive: bool,
        cb: impl Fn(&notify::Event) + Send + Sync + 'static,
    ) -> anyhow::Result<FsWatcherRegistration> {
        let path = SanitizedPath::from_arc(path);
        let key = WatchKey::for_registration(&path, case_insensitive);
        let callback: Arc<dyn Fn(&notify::Event) + Send + Sync> = Arc::new(cb);
        if mode == WatcherMode::Native {
            self.budget.ensure_initial_measurement();
        }

        let key_guard = self.lock_key(&key);
        {
            let mut state = self.state.lock();
            if let Some((registered_mode, has_os_watcher)) =
                state.joinable_registration(&key, &path, mode)
            {
                let id =
                    state.register_watcher(key, path, registered_mode, callback, has_os_watcher);
                return Ok(FsWatcherRegistration {
                    id,
                    mode: registered_mode,
                });
            }
        }

        let new_watch = self.watch_new_path(&path, mode);
        let refused_path = new_watch.hit_watch_limit.then(|| path.clone());
        let registration = new_watch.mode.map(|registered_mode| {
            let id = self
                .state
                .lock()
                .register_watcher(key, path, registered_mode, callback, true);
            FsWatcherRegistration {
                id,
                mode: registered_mode,
            }
        });
        drop(key_guard);

        // Learning scans `/proc`, and trimming to the learned limit can move thousands of
        // paths, so neither happens under the key lock or on the caller's thread.
        if let Some(refused_path) = refused_path
            && self.budget.is_enabled()
            && self
                .budget
                .learn_limit_from_watch_limit_error(refused_path.as_path())
        {
            self.request_maintenance();
        }
        if let Ok(registration) = &registration
            && (registration.mode == WatcherMode::Fallback
                || (registration.mode == WatcherMode::Native && self.budget.is_enabled()))
        {
            self.ensure_background_thread();
        }
        registration
    }

    /// Starts watching a path that has no registration yet. Paths that can't get a native
    /// watch are polled instead of being skipped, so they never have to be retried.
    fn watch_new_path(&self, path: &SanitizedPath, mode: WatcherMode) -> NewWatch {
        if mode != WatcherMode::Native {
            return NewWatch {
                mode: self.watch(path.as_path(), mode).map(|()| mode),
                hit_watch_limit: false,
            };
        }

        if !self.budget.try_reserve_native() {
            return NewWatch {
                mode: self.watch_fallback(path),
                hit_watch_limit: false,
            };
        }

        // Creating the inotify instance fails once the user's instance limit is reached; the
        // path is still polled then.
        if let Err(error) = self.ensure_native_watcher() {
            self.budget.release_native();
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                log::warn!("native file watching is unavailable, polling instead: {error:#}");
            }
            return NewWatch {
                mode: self.watch_fallback(path),
                hit_watch_limit: false,
            };
        }

        match self.watch(path.as_path(), WatcherMode::Native) {
            Ok(()) => NewWatch {
                mode: Ok(WatcherMode::Native),
                hit_watch_limit: false,
            },
            Err(error) if is_max_files_watch_error(&error) => {
                self.budget.release_native();
                NewWatch {
                    mode: self.watch_fallback(path),
                    hit_watch_limit: true,
                }
            }
            Err(error) if is_unsupported_native_watch_error(&error) => {
                self.budget.release_native();
                NewWatch {
                    mode: self.watch_poll_instead_of_native(path, &error),
                    hit_watch_limit: false,
                }
            }
            Err(error) => {
                self.budget.release_native();
                NewWatch {
                    mode: Err(error),
                    hit_watch_limit: false,
                }
            }
        }
    }

    /// Polls a path whose volume has no working native watch. Polling is recursive, which is
    /// what the native watch would have been on the platforms that report this (Windows), so
    /// the registration still covers descendants.
    fn watch_poll_instead_of_native(
        &self,
        path: &SanitizedPath,
        error: &anyhow::Error,
    ) -> anyhow::Result<WatcherMode> {
        log::info!(
            "Native file watching is not supported for {} ({error}); using poll watcher ({}ms interval)",
            path.as_path().display(),
            poll_interval().as_millis(),
        );
        telemetry::event!(
            "fs_watcher_poll",
            path = path.as_path().display().to_string()
        );
        self.watch(path.as_path(), WatcherMode::Poll)?;
        Ok(WatcherMode::Poll)
    }

    fn watch_fallback(&self, path: &SanitizedPath) -> anyhow::Result<WatcherMode> {
        self.watch(path.as_path(), WatcherMode::Fallback)?;
        Ok(WatcherMode::Fallback)
    }

    fn enqueue(&self, mode: WatcherMode, event: Result<notify::Event, notify::Error>) {
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
        self.event_tx.try_send((mode, event)).ok();
    }

    fn dispatch(&self, mode: WatcherMode, event: Result<notify::Event, notify::Error>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                log::warn!("watcher error for {mode:?}: {error}");
                return;
            }
        };

        log::trace!("global handle event for {mode:?}: {event:?}");

        let callbacks = {
            let state = self.state.lock();
            if event.need_rescan() {
                // A native overflow also covers paths that were just handed to the poller, whose
                // native events may be among the lost ones.
                let callbacks = state
                    .watchers
                    .values()
                    .filter(|registration| {
                        registration.mode == mode
                            || (mode == WatcherMode::Native
                                && registration.mode == WatcherMode::Fallback)
                    })
                    .map(|registration| registration.callback.clone())
                    .collect::<Vec<_>>();
                log::warn!(
                    "filesystem watcher lost sync for {mode:?}; scheduling rescans for {} registrations",
                    callbacks.len()
                );
                callbacks
            } else {
                let targets = state.dispatch_targets(mode);
                let include_folded = targets
                    .iter()
                    .flatten()
                    .any(|path_registrations| path_registrations.has_folded_entries());
                let mut ids = Vec::new();
                for path in &event.paths {
                    let keys = ancestor_keys(SanitizedPath::new(path), include_folded);
                    for path_registrations in targets.iter().flatten() {
                        path_registrations.watcher_ids_for(&keys, &mut ids);
                    }
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
        &self,
        first: DispatchEvent,
        event_rx: &async_channel::Receiver<DispatchEvent>,
    ) {
        // A single backend overflow can enqueue many rescan markers. One rescan
        // per mode covers the entire drained batch; ordinary events still run.
        let mut native_rescan_dispatched = false;
        let mut poll_rescan_dispatched = false;
        let mut fallback_rescan_dispatched = false;

        for (mode, event) in
            std::iter::once(first).chain(std::iter::from_fn(|| event_rx.try_recv().ok()))
        {
            let rescan_dispatched = match mode {
                WatcherMode::Native => &mut native_rescan_dispatched,
                WatcherMode::Poll => &mut poll_rescan_dispatched,
                WatcherMode::Fallback => &mut fallback_rescan_dispatched,
            };
            if event.as_ref().is_ok_and(notify::Event::need_rescan) {
                if *rescan_dispatched {
                    continue;
                }
                *rescan_dispatched = true;
            }

            self.dispatch(mode, event);
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
        let Some((path, mode)) = self.state.lock().remove_registration(id) else {
            return;
        };
        self.unwatch(path.as_path(), mode).log_err();
        if mode == WatcherMode::Native {
            self.budget.release_native();
        }
    }

    fn watch(&self, path: &Path, mode: WatcherMode) -> anyhow::Result<()> {
        match mode {
            WatcherMode::Native => {
                self.ensure_native_watcher()?;
                self.native_watcher
                    .lock()
                    .as_mut()
                    .expect("native watcher initialized")
                    .watch(
                        path,
                        if mode.is_recursive() {
                            notify::RecursiveMode::Recursive
                        } else {
                            notify::RecursiveMode::NonRecursive
                        },
                    )?;
            }
            WatcherMode::Poll => {
                self.ensure_poll_watcher()?;
                self.poll_watcher
                    .lock()
                    .as_mut()
                    .expect("poll watcher initialized")
                    .watch(path, notify::RecursiveMode::Recursive)?;
            }
            WatcherMode::Fallback => self.fallback_watcher.watch(path)?,
        }

        Ok(())
    }

    fn unwatch(&self, path: &Path, mode: WatcherMode) -> anyhow::Result<()> {
        let watcher = match mode {
            WatcherMode::Native => self
                .native_watcher
                .lock()
                .as_mut()
                .map(|watcher| watcher.unwatch(path)),
            WatcherMode::Poll => self
                .poll_watcher
                .lock()
                .as_mut()
                .map(|watcher| watcher.unwatch(path)),
            WatcherMode::Fallback => Some(self.fallback_watcher.unwatch(path)),
        };

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

    fn demote_native_watches_over_learned_limit(&self) {
        let Some(learned_limit) = self.budget.learned_limit() else {
            return;
        };
        let excess = native_watches_over_learned_limit(self.budget.native_watches(), learned_limit);
        if excess > 0 {
            self.demote_native_watches(excess);
        }
    }

    fn demote_native_watches(&self, max_demotions: usize) -> usize {
        let candidates = self
            .state
            .lock()
            .native_path_registrations
            .transition_candidates(TransitionOrder::DeepestFirst);
        let mut demoted = 0;
        for key in candidates {
            if demoted >= max_demotions {
                break;
            }
            if self.demote_native_watch(&key) {
                demoted += 1;
            }
        }
        if demoted > 0 {
            self.ensure_background_thread();
        }
        demoted
    }

    fn demote_native_watch(&self, key: &WatchKey) -> bool {
        let _key_guard = self.lock_key(key);
        let Some(watched_path) = self
            .state
            .lock()
            .native_path_registrations
            .watched_path(key)
        else {
            return false;
        };
        // The poller takes its baseline before the native watch goes away, so that no change
        // can fall between the two.
        if let Err(error) = self.watch(watched_path.as_path(), WatcherMode::Fallback) {
            log::debug!(
                "keeping the native watch on {:?}: {error:#}",
                watched_path.as_path()
            );
            return false;
        }
        self.state
            .lock()
            .move_path_registration(key, WatcherMode::Native, WatcherMode::Fallback);
        self.unwatch(watched_path.as_path(), WatcherMode::Native)
            .log_err();
        self.budget.release_native();
        true
    }

    fn promote_fallback_watches(&self, max_promotions: usize) -> usize {
        let candidates = self
            .state
            .lock()
            .fallback_path_registrations
            .transition_candidates(TransitionOrder::ShallowestFirst);
        let mut promoted = 0;
        let mut refused_path = None;
        for key in candidates {
            if promoted >= max_promotions {
                break;
            }
            // Other Zed processes may be promoting at the same time, so the room is measured
            // again every few promotions.
            if promoted > 0 && promoted % PROMOTIONS_PER_MEASUREMENT == 0 {
                let reading = self.budget.measure();
                if reading
                    .limit
                    .is_none_or(|limit| promotion_capacity(reading.usage, limit) == 0)
                {
                    break;
                }
            }
            match self.promote_fallback_watch(&key) {
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
            && self
                .budget
                .learn_limit_from_watch_limit_error(refused_path.as_path())
        {
            self.demote_native_watches_over_learned_limit();
        }
        promoted
    }

    fn promote_fallback_watch(&self, key: &WatchKey) -> PromotionOutcome {
        let _key_guard = self.lock_key(key);
        let Some(watched_path) = self
            .state
            .lock()
            .fallback_path_registrations
            .watched_path(key)
        else {
            return PromotionOutcome::Skipped;
        };
        if self.fallback_watcher.has_vanished(watched_path.as_path()) {
            return PromotionOutcome::Skipped;
        }
        if !self.budget.try_reserve_native() {
            return PromotionOutcome::BudgetExhausted;
        }
        match self.watch(watched_path.as_path(), WatcherMode::Native) {
            Ok(()) => {
                self.state.lock().move_path_registration(
                    key,
                    WatcherMode::Fallback,
                    WatcherMode::Native,
                );
                // Changes made since the poller's last pass happened before the native watch
                // existed; one last poll reports them.
                self.fallback_watcher.poll_path(watched_path.as_path());
                self.unwatch(watched_path.as_path(), WatcherMode::Fallback)
                    .log_err();
                PromotionOutcome::Promoted
            }
            Err(error) if is_max_files_watch_error(&error) => {
                self.budget.release_native();
                PromotionOutcome::WatchLimitReached(watched_path)
            }
            Err(error) => {
                self.budget.release_native();
                log::debug!("keeping {:?} on polling: {error:#}", watched_path.as_path());
                self.state
                    .lock()
                    .fallback_path_registrations
                    .mark_native_unavailable(key);
                PromotionOutcome::Skipped
            }
        }
    }

    fn run_budget_maintenance(&self, promote: bool) {
        if !self.budget.is_enabled() {
            return;
        }
        let (native_paths, fallback_paths) = {
            let state = self.state.lock();
            (
                state.native_path_registrations.len(),
                state.fallback_path_registrations.len(),
            )
        };
        if native_paths == 0 && fallback_paths == 0 {
            return;
        }

        // Promotions need current numbers; otherwise the estimate is enough until a measurement
        // is due.
        if promote || self.budget.measurement_due(Instant::now()) {
            self.budget.measure();
        }
        // Trimming to a limit learned from ENOSPC comes first, since that is what makes room
        // below the limit.
        self.demote_native_watches_over_learned_limit();
        let reading = self.budget.reading();
        let Some(limit) = reading.limit else {
            return;
        };
        let native_watches = self.budget.native_watches();
        if should_yield_native_watches(reading.usage, limit) {
            if native_watches > 0 {
                if self.budget.begin_yielding() {
                    log::warn!(
                        "only {} of this user's {limit} inotify watches are free; moving {native_watches} watched paths to polling so that other programs can keep watching files",
                        limit.saturating_sub(reading.usage)
                    );
                }
                self.demote_native_watches(usize::MAX);
            }
            return;
        }

        let over_share = native_watches_over_share(reading.usage, limit, native_watches);
        if over_share > 0 {
            log::info!(
                "{} of this user's {limit} inotify watches are in use, well over Zed's share; moving {over_share} watched paths to polling",
                reading.usage
            );
            self.demote_native_watches(over_share);
            return;
        }

        if promote && fallback_paths > 0 {
            let capacity = promotion_capacity(reading.usage, limit);
            if capacity > 0 {
                let promoted = self.promote_fallback_watches(capacity);
                if promoted > 0 {
                    log::info!("moved {promoted} polled paths back to inotify watches");
                    self.budget.end_episode();
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

    /// Asks the background thread to run maintenance right away.
    fn request_maintenance(&self) {
        *self.maintenance_requested.lock() = true;
        self.maintenance_wake.notify_one();
    }

    fn take_maintenance_request(&self) -> bool {
        std::mem::replace(&mut *self.maintenance_requested.lock(), false)
    }

    fn wait_for_maintenance_request(&self, deadline: Instant) -> bool {
        let mut requested = self.maintenance_requested.lock();
        if !*requested {
            self.maintenance_wake.wait_until(&mut requested, deadline);
        }
        std::mem::replace(&mut *requested, false)
    }

    fn ensure_background_thread(&self) {
        if !self.spawn_background_thread {
            return;
        }
        let mut background_thread = self.background_thread.lock();
        match *background_thread {
            BackgroundThread::Running => return,
            BackgroundThread::FailedAt(failed_at)
                if failed_at.elapsed() < BACKGROUND_THREAD_RETRY_INTERVAL =>
            {
                return;
            }
            BackgroundThread::NotStarted | BackgroundThread::FailedAt(_) => {}
        }
        match std::thread::Builder::new()
            .name("fs-watcher-fallback".to_owned())
            .spawn(|| run_background_thread(global_watcher()))
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

    fn ensure_native_watcher(&self) -> anyhow::Result<()> {
        // The lock is held across creation: with a check-then-insert under two
        // separate lock acquisitions, concurrent callers could each create a
        // watcher and the loser's insert would silently drop the winner's
        // watcher along with every path registered on it.
        let mut native_watcher = self.native_watcher.lock();
        if native_watcher.is_none() {
            // CORE excludes Access events, which Zed discards anyway. Without this,
            // the default mask subscribes to inotify OPEN/CLOSE_* on Linux, so every
            // file read in a watched directory would queue events, increasing the
            // risk of queue overflows (and thus full rescans) under read-heavy
            // workloads like grep or language server indexing.
            let config = notify::Config::default().with_event_kinds(notify::EventKindMask::CORE);
            let watcher = <notify::RecommendedWatcher as notify::Watcher>::new(
                |event| global_watcher().enqueue(WatcherMode::Native, event),
                config,
            )?;
            *native_watcher = Some(Box::new(watcher));
        }
        Ok(())
    }

    fn ensure_poll_watcher(&self) -> anyhow::Result<()> {
        let mut poll_watcher = self.poll_watcher.lock();
        if poll_watcher.is_none() {
            let config = notify::Config::default().with_poll_interval(*POLL_INTERVAL);
            // On Windows the poller serves the 9P share of a WSL distribution, where every link
            // the distribution made is one Windows cannot follow: following links would make each
            // pass report an error for each of them. The link itself is still compared, and
            // neither native backend follows links either.
            #[cfg(target_os = "windows")]
            let config = config.with_follow_symlinks(false);
            let watcher = notify::PollWatcher::new(
                |event| global_watcher().enqueue(WatcherMode::Poll, event),
                config,
            )?;
            *poll_watcher = Some(Box::new(watcher));
        }
        Ok(())
    }
}

/// Polls the fallback paths and keeps the watch budget up to date for the global watcher.
fn run_background_thread(watcher: &'static GlobalWatcher) {
    let started = Instant::now();
    let mut schedule = MaintenanceSchedule {
        next_tick: started + MAINTENANCE_INTERVAL,
        next_promotion: started + PROMOTION_INTERVAL,
    };
    let mut next_pass = started + poll_interval();
    loop {
        let wake_at = next_pass.min(schedule.next_tick);
        if watcher.wait_for_maintenance_request(wake_at) {
            schedule.next_tick = Instant::now();
        }
        watcher.run_scheduled_maintenance(&mut schedule);
        if Instant::now() < next_pass {
            continue;
        }

        let pass_started = Instant::now();
        let mut pacer = PollPacer::new();
        let operation_budget = sweep_operation_budget(poll_interval());
        watcher
            .fallback_watcher
            .poll_pass(Some(operation_budget), &mut |operations| {
                if let Some(delay) = pacer.record(operations) {
                    std::thread::sleep(delay);
                }
                // A pass over a large tree can take far longer than the maintenance interval.
                if watcher.take_maintenance_request() {
                    schedule.next_tick = Instant::now();
                }
                watcher.run_scheduled_maintenance(&mut schedule);
            });
        next_pass = (pass_started + poll_interval()).max(Instant::now());
    }
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

static POLL_INTERVAL: LazyLock<Duration> = LazyLock::new(|| {
    let poll_ms: u64 = std::env::var("ZED_FILE_WATCHER_POLL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
        .clamp(500, 30000);
    Duration::from_millis(poll_ms)
});

pub fn poll_interval() -> Duration {
    *POLL_INTERVAL
}

static FS_WATCHER_INSTANCE: OnceLock<GlobalWatcher> = OnceLock::new();

fn global_watcher() -> &'static GlobalWatcher {
    FS_WATCHER_INSTANCE.get_or_init(|| {
        let (event_tx, event_rx) = async_channel::unbounded::<DispatchEvent>();
        std::thread::Builder::new()
            .name("fs-watcher-dispatch".to_owned())
            .spawn(move || {
                while let Ok(first) = event_rx.recv_blocking() {
                    global_watcher().dispatch_batch(first, &event_rx);
                }
            })
            .expect("failed to spawn fs watcher dispatch thread");
        GlobalWatcher::new(
            event_tx,
            None,
            None,
            Arc::new(|event| global_watcher().enqueue(WatcherMode::Fallback, Ok(event))),
            // Only inotify charges one watch per directory against a small per-user limit;
            // FSEvents and ReadDirectoryChangesW watch whole trees with one handle.
            WatchBudget::new(cfg!(target_os = "linux"), Arc::new(ProcfsBudgetSource)),
            true,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{
        fallback_watcher::FallbackEventSink,
        watch_budget::{InotifyUsage, WatchBudgetSource},
    };
    use std::{collections::HashSet, path::PathBuf};

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

    fn test_watcher(poll_watcher: Arc<Mutex<FakeWatchBackend>>) -> GlobalWatcher {
        test_watcher_with_backends(None, Some(poll_watcher))
    }

    fn test_watcher_with_backends(
        native_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
        poll_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
    ) -> GlobalWatcher {
        // Tests call `dispatch` directly to exercise dispatch synchronously,
        // rather than going through the OS watcher callbacks, so nothing is ever
        // sent on this channel; the receiver can just be dropped.
        let (event_tx, _event_rx) = async_channel::unbounded();
        GlobalWatcher::new(
            event_tx,
            native_watcher
                .map(|watcher| Box::new(SharedFakeWatchBackend(watcher)) as Box<dyn WatchBackend>),
            poll_watcher
                .map(|watcher| Box::new(SharedFakeWatchBackend(watcher)) as Box<dyn WatchBackend>),
            Arc::new(|_| {}),
            WatchBudget::new(false, Arc::new(ProcfsBudgetSource)),
            false,
        )
    }

    /// A watcher with the watch budget enabled. Events of the fallback poller are collected
    /// rather than dispatched, so tests can deliver them after running a pass.
    fn test_watcher_with_budget(
        native_backend: Arc<Mutex<FakeWatchBackend>>,
        budget_source: Arc<FakeBudgetSource>,
    ) -> (GlobalWatcher, Arc<Mutex<Vec<notify::Event>>>) {
        let (event_tx, _event_rx) = async_channel::unbounded();
        let fallback_events = Arc::new(Mutex::new(Vec::new()));
        let fallback_event_sink: FallbackEventSink = {
            let fallback_events = fallback_events.clone();
            Arc::new(move |event| fallback_events.lock().push(event))
        };
        let watcher = GlobalWatcher::new(
            event_tx,
            Some(Box::new(SharedFakeWatchBackend(native_backend))),
            None,
            fallback_event_sink,
            WatchBudget::new(true, budget_source),
            false,
        );
        (watcher, fallback_events)
    }

    fn deliver_fallback_events(
        watcher: &GlobalWatcher,
        fallback_events: &Mutex<Vec<notify::Event>>,
    ) {
        let events = std::mem::take(&mut *fallback_events.lock());
        for event in events {
            watcher.dispatch(WatcherMode::Fallback, Ok(event));
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

    fn registration_mode(
        watcher: &GlobalWatcher,
        id: WatcherRegistrationId,
    ) -> Option<WatcherMode> {
        watcher
            .state
            .lock()
            .watchers
            .get(&id)
            .map(|registration| registration.mode)
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

    fn watch_all(
        watcher: &GlobalWatcher,
        directories: &[PathBuf],
        callback: impl Fn(&notify::Event) + Send + Sync + Clone + 'static,
    ) -> Vec<FsWatcherRegistration> {
        directories
            .iter()
            .map(|directory| {
                watcher
                    .add(
                        directory.as_path().into(),
                        WatcherMode::Native,
                        false,
                        callback.clone(),
                    )
                    .expect("add watch")
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
        let watcher = test_watcher(backend.clone());
        let parent = Arc::<Path>::from(Path::new("/repo"));
        let child = Arc::<Path>::from(Path::new("/repo/foo.csproj"));

        let parent_registration = watcher
            .add(parent.as_ref().into(), WatcherMode::Poll, false, |_| {})
            .expect("add parent watch");
        let child_registration = watcher
            .add(child.as_ref().into(), WatcherMode::Poll, false, |_| {})
            .expect("add covered child watch");

        watcher.remove(parent_registration.id);
        watcher.remove(child_registration.id);

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
        let watcher = FsWatcher::new(cx.executor(), tx, pending_path_events.clone());

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
    fn native_watch_limit_without_a_budget_falls_back_to_polling() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories =
            make_directories(temp_dir.path(), ["first".to_owned(), "second".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let watcher = test_watcher_with_backends(Some(native_backend.clone()), None);

        let registrations = watch_all(&watcher, &directories, |_| {});

        // Both paths are watched by polling instead of being skipped and retried later.
        assert!(
            registrations
                .iter()
                .all(|registration| registration.mode == WatcherMode::Fallback)
        );
        assert_eq!(native_backend.lock().watch_calls, directories);
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

    /// A volume that isn't a WSL share but still has no `ReadDirectoryChangesW` is caught by
    /// the failure of the native watch itself.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_native_watch_the_volume_cannot_serve_falls_back_to_polling() {
        const ERROR_INVALID_FUNCTION: i32 = 1;
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["share".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_io_os_error: Some(ERROR_INVALID_FUNCTION),
            ..Default::default()
        }));
        let poll_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher =
            test_watcher_with_backends(Some(native_backend.clone()), Some(poll_backend.clone()));

        let registrations = watch_all(&watcher, &directories, |_| {});

        assert_eq!(
            registrations.first().map(|registration| registration.mode),
            Some(WatcherMode::Poll)
        );
        assert_eq!(native_watched_paths(&native_backend), HashSet::new());
        assert_eq!(
            native_watched_paths(&poll_backend),
            directories.iter().cloned().collect::<HashSet<_>>()
        );
    }

    /// Only "this volume cannot do that" is answered with polling: an error polling wouldn't
    /// fix still reaches the caller.
    #[cfg(target_os = "windows")]
    #[test]
    fn another_native_watch_error_is_reported_rather_than_polled() {
        const ERROR_ACCESS_DENIED: i32 = 5;
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["denied".to_owned()]);
        let directory = directories.first().expect("one directory").clone();
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_io_os_error: Some(ERROR_ACCESS_DENIED),
            ..Default::default()
        }));
        let poll_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(native_backend), Some(poll_backend.clone()));

        let result = watcher.add(
            directory.as_path().into(),
            WatcherMode::Native,
            false,
            |_| {},
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
    #[test]
    #[ignore]
    fn e2e_wsl_share_changes_reach_the_watcher() {
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
        let file_id_error =
            smol::block_on(crate::file_id(&link)).expect_err("the file id needs the target");
        assert_eq!(
            file_id_error
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error),
            Some(ERROR_PATH_NOT_FOUND),
            "this is the error `worktree` recognizes such a link by"
        );

        let (tx, _rx) = async_channel::unbounded();
        let pending_path_events: Arc<Mutex<Vec<PathEvent>>> = Default::default();
        let registration = register_existing_path(
            share.as_path().into(),
            case_insensitive_path(&share),
            tx,
            pending_path_events.clone(),
        )
        .expect("watching the share");

        run("touch /tmp/zed-4a-watch/from-wsl.txt".to_owned());

        // Long enough for several poll passes over 9P, which is slower than a local volume.
        let deadline = Instant::now() + Duration::from_secs(30);
        let reported = loop {
            let events = pending_path_events.lock().clone();
            if events
                .iter()
                .any(|event| event.path.ends_with("from-wsl.txt"))
            {
                break events;
            }
            if Instant::now() >= deadline {
                break events;
            }
            std::thread::sleep(Duration::from_millis(250));
        };

        global_watcher().remove(registration.id);
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
            test_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);

        // A quarter of the limit of 40 is 10 watches for the whole user.
        let native_registrations = registrations
            .iter()
            .filter(|registration| registration.mode == WatcherMode::Native)
            .count();
        assert_eq!(native_registrations, 10);
        assert_eq!(native_backend.lock().watched_paths.len(), 10);
        assert_eq!(watcher.budget.native_watches(), 10);

        let fallback_directory = registrations
            .iter()
            .zip(&directories)
            .find(|(registration, _)| registration.mode == WatcherMode::Fallback)
            .map(|(_, directory)| directory)
            .expect("paths beyond the budget are polled");
        let file = fallback_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_watcher.poll_once();
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
            test_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);
        for _ in 0..5 {
            watcher.run_budget_maintenance(true);
        }

        assert!(
            registrations
                .iter()
                .all(|registration| registration.mode == WatcherMode::Fallback)
        );
        assert_eq!(native_backend.lock().watch_calls, &directories[..1]);
        assert_eq!(watcher.budget.learned_limit(), Some(0));

        let file = directories[2].join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_watcher.poll_once();
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
            test_watcher_with_budget(native_backend.clone(), budget_source);
        let (fired, callback) = path_recorder();

        let registrations = watch_all(&watcher, &directories, callback);
        // Trimming to the learned limit is left to the maintenance thread.
        watcher.run_budget_maintenance(false);

        assert_eq!(watcher.budget.learned_limit(), Some(40));
        assert_eq!(native_backend.lock().watched_paths.len(), 10);
        assert_eq!(watcher.budget.native_watches(), 10);
        assert_eq!(native_backend.lock().watch_calls.len(), 41);
        {
            let state = watcher.state.lock();
            assert_eq!(state.native_path_registrations.len(), 10);
            assert_eq!(state.fallback_path_registrations.len(), 50);
            assert_eq!(state.watchers.len(), 60);
        }

        let demoted_directory = registrations
            .iter()
            .zip(&directories)
            .find(|(registration, _)| {
                registration.mode == WatcherMode::Native
                    && registration_mode(&watcher, registration.id) == Some(WatcherMode::Fallback)
            })
            .map(|(_, directory)| directory)
            .expect("some native watches were handed to the poller");
        let file = demoted_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        watcher.fallback_watcher.poll_once();
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
            test_watcher_with_budget(native_backend.clone(), budget_source.clone());
        let (fired, callback) = path_recorder();

        let registration = watcher
            .add(
                directory.as_path().into(),
                WatcherMode::Native,
                false,
                callback,
            )
            .expect("add watch");
        assert_eq!(registration.mode, WatcherMode::Native);

        // Other programs leave less than an eighth of the limit free.
        *budget_source.other_programs_usage.lock() = 990;
        watcher.budget.measure();
        watcher.run_budget_maintenance(false);
        assert!(native_backend.lock().watched_paths.is_empty());
        assert_eq!(
            registration_mode(&watcher, registration.id),
            Some(WatcherMode::Fallback)
        );
        assert_eq!(watcher.budget.native_watches(), 0);

        let polled_file = directory.join("while-polled.txt");
        write_settled_file(&polled_file);
        watcher.fallback_watcher.poll_once();
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
            registration_mode(&watcher, registration.id),
            Some(WatcherMode::Native)
        );
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(std::mem::take(&mut *fired.lock()), vec![handover_file]);

        // The poller no longer reports the path once it is watched natively again.
        write_settled_file(&directory.join("after-promotion.txt"));
        watcher.fallback_watcher.poll_once();
        assert!(fallback_events.lock().is_empty());

        watcher.remove(registration.id);
        assert!(native_backend.lock().watched_paths.is_empty());
        assert_eq!(watcher.budget.native_watches(), 0);
        assert!(watcher.state.lock().watchers.is_empty());
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
            test_watcher_with_budget(native_backend.clone(), budget_source);

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
            test_watcher_with_budget(native_backend.clone(), budget_source.clone());

        let registrations = watch_all(&watcher, &directories, |_| {});
        assert!(
            registrations
                .iter()
                .all(|registration| registration.mode == WatcherMode::Fallback)
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
        let (watcher, fallback_events) = test_watcher_with_budget(native_backend, budget_source);
        let (first_fired, first_callback) = path_recorder();
        let (second_fired, second_callback) = path_recorder();

        let first = watch_all(&watcher, &directories, first_callback);
        let second = watch_all(&watcher, &directories, second_callback);
        assert_eq!(first[0].mode, WatcherMode::Fallback);
        assert_eq!(second[0].mode, WatcherMode::Fallback);

        let file = directories[0].join("created.txt");
        write_settled_file(&file);
        watcher.fallback_watcher.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(*first_fired.lock(), vec![file.clone()]);
        assert_eq!(*second_fired.lock(), vec![file]);

        // The poll only stops once the last registration is gone.
        watcher.remove(first[0].id);
        write_settled_file(&directories[0].join("second.txt"));
        watcher.fallback_watcher.poll_once();
        assert_eq!(fallback_events.lock().len(), 1);
        fallback_events.lock().clear();

        watcher.remove(second[0].id);
        write_settled_file(&directories[0].join("third.txt"));
        watcher.fallback_watcher.poll_once();
        assert!(fallback_events.lock().is_empty());
    }

    /// Hands every native watch over to the poller, as when other programs are about to run out
    /// of inotify watches, and then frees the watches again.
    fn yield_native_watches(watcher: &GlobalWatcher, budget_source: &FakeBudgetSource) {
        let limit = budget_source.limit.lock().unwrap_or_default();
        *budget_source.other_programs_usage.lock() = limit;
        watcher.budget.measure();
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
            test_watcher_with_budget(native_backend, budget_source.clone());
        let (fired, callback) = path_recorder();
        let registration = watcher
            .add(
                directory.as_path().into(),
                WatcherMode::Native,
                false,
                callback,
            )
            .expect("add watch");

        yield_native_watches(&watcher, &budget_source);
        assert_eq!(
            registration_mode(&watcher, registration.id),
            Some(WatcherMode::Fallback)
        );

        // The native watcher reported this before the hand-over; dispatch runs later.
        let file = directory.join("queued.txt");
        watcher.dispatch(
            WatcherMode::Native,
            Ok(
                notify::Event::new(EventKind::Create(notify::event::CreateKind::File))
                    .add_path(file.clone()),
            ),
        );
        assert_eq!(*fired.lock(), vec![file]);
    }

    #[test]
    fn native_rescan_reaches_paths_handed_to_the_poller() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(temp_dir.path(), ["a".to_owned(), "b".to_owned()]);
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, _fallback_events) =
            test_watcher_with_budget(native_backend, budget_source.clone());
        let (first_fired, first_callback) = fired_count();
        let (second_fired, second_callback) = fired_count();
        watch_all(&watcher, &directories[..1], first_callback);
        watch_all(&watcher, &directories[1..], second_callback);

        yield_native_watches(&watcher, &budget_source);
        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        watcher.dispatch(WatcherMode::Native, Ok(rescan));

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
        let (watcher, _fallback_events) = test_watcher_with_budget(native_backend, budget_source);
        let registrations = watch_all(&watcher, &directories, |_| {});
        assert_eq!(watcher.budget.native_watches(), 10);

        let polled = registrations
            .iter()
            .find(|registration| registration.mode == WatcherMode::Fallback)
            .expect("the eleventh path is polled");
        watcher.remove(polled.id);
        assert_eq!(watcher.budget.native_watches(), 10);

        let native = registrations
            .iter()
            .find(|registration| registration.mode == WatcherMode::Native)
            .expect("the first paths are watched natively");
        watcher.remove(native.id);
        assert_eq!(watcher.budget.native_watches(), 9);
    }

    #[test]
    fn every_registration_of_a_path_survives_demotion_and_promotion() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directory = watched_directory(temp_dir.path());
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let budget_source = FakeBudgetSource::new(Some(1000), native_backend.clone());
        let (watcher, fallback_events) =
            test_watcher_with_budget(native_backend.clone(), budget_source.clone());
        let (first_fired, first_callback) = path_recorder();
        let (second_fired, second_callback) = path_recorder();
        let first = watcher
            .add(
                directory.as_path().into(),
                WatcherMode::Native,
                false,
                first_callback,
            )
            .expect("add first watch");
        let second = watcher
            .add(
                directory.as_path().into(),
                WatcherMode::Native,
                false,
                second_callback,
            )
            .expect("add second watch");
        assert_eq!(native_backend.lock().watch_calls.len(), 1);

        yield_native_watches(&watcher, &budget_source);
        for id in [first.id, second.id] {
            assert_eq!(registration_mode(&watcher, id), Some(WatcherMode::Fallback));
        }
        let polled_file = directory.join("polled.txt");
        write_settled_file(&polled_file);
        watcher.fallback_watcher.poll_once();
        deliver_fallback_events(&watcher, &fallback_events);
        assert_eq!(
            std::mem::take(&mut *first_fired.lock()),
            vec![polled_file.clone()]
        );
        assert_eq!(std::mem::take(&mut *second_fired.lock()), vec![polled_file]);

        let handover_file = directory.join("handover.txt");
        write_settled_file(&handover_file);
        watcher.run_budget_maintenance(true);
        for id in [first.id, second.id] {
            assert_eq!(registration_mode(&watcher, id), Some(WatcherMode::Native));
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

    fn recording_watcher() -> (GlobalWatcher, Arc<Mutex<Vec<String>>>) {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend), None);
        let fired = Arc::new(Mutex::new(Vec::new()));
        for dir in ["/repo/a", "/repo/a/nested", "/repo/b"] {
            let fired = fired.clone();
            let label = dir.to_owned();
            watcher
                .add(
                    Arc::<Path>::from(Path::new(dir)),
                    WatcherMode::Native,
                    false,
                    move |_| {
                        fired.lock().push(label.clone());
                    },
                )
                .expect("add watch");
        }
        (watcher, fired)
    }

    #[test]
    fn event_dispatches_only_to_registrations_covering_its_path() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/repo/a/file.txt")));

        // Only the directory containing the file resolves; siblings stay untouched.
        assert_eq!(*fired.lock(), vec!["/repo/a".to_owned()]);
    }

    #[test]
    fn event_dispatches_to_every_ancestor_registration() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("/repo/a/nested/file.txt")),
        );

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
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/Project").into(),
                WatcherMode::Native,
                true,
                cb,
            )
            .expect("add");

        // Event arrives lowercased (as TSGO/macOS may report it).
        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("/repo/project/file.txt")),
        );
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_insensitive_registration_survives_case_only_rename() {
        let (fired, cb) = fired_count();
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/Proj").into(),
                WatcherMode::Native,
                true,
                cb,
            )
            .expect("add");

        // The watched directory was renamed to a different casing; events now
        // arrive under the new spelling.
        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_sensitive_registration_ignores_differently_cased_event() {
        let (fired, cb) = fired_count();
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/proj").into(),
                WatcherMode::Native,
                false,
                cb,
            )
            .expect("add");

        // On a case-sensitive volume these are genuinely different directories.
        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 0);
    }

    #[test]
    fn differently_cased_adds_dedupe_on_case_insensitive_volume() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend.clone()), None);
        watcher
            .add(
                Path::new("/Repo/Proj").into(),
                WatcherMode::Native,
                true,
                |_| {},
            )
            .expect("add");
        watcher
            .add(
                Path::new("/repo/proj").into(),
                WatcherMode::Native,
                true,
                |_| {},
            )
            .expect("add");

        // The second, differently-cased spelling reuses the same OS watch.
        assert_eq!(backend.lock().watch_calls.len(), 1);
    }

    #[test]
    fn recursive_parent_covers_differently_cased_child() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher(backend.clone());
        watcher
            .add(Path::new("/Repo").into(), WatcherMode::Poll, true, |_| {})
            .expect("add");
        watcher
            .add(
                Path::new("/repo/child").into(),
                WatcherMode::Poll,
                true,
                |_| {},
            )
            .expect("add");

        // The child is covered by the recursive parent despite the case mismatch.
        assert_eq!(backend.lock().watch_calls, vec![PathBuf::from("/Repo")]);
    }

    #[test]
    fn rescan_event_broadcasts_to_all_registrations_of_the_same_mode() {
        let (watcher, fired) = recording_watcher();

        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        watcher.dispatch(WatcherMode::Native, Ok(rescan));

        // A pathless rescan may have missed events anywhere, so every registration
        // of that mode resyncs, regardless of which directory it watches.
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

        event_tx
            .try_send((WatcherMode::Native, Ok(rescan())))
            .unwrap();
        event_tx
            .try_send((WatcherMode::Native, Ok(modify_event("/repo/a/file.txt"))))
            .unwrap();
        watcher.dispatch_batch((WatcherMode::Native, Ok(rescan())), &event_rx);

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
    #[cfg(target_os = "windows")]
    fn event_dispatches_when_reported_path_has_verbatim_prefix() {
        // `notify` (and Windows APIs generally) can report a changed path using the
        // verbatim `\\?\` long-path form even when the directory was registered
        // without it.
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend), None);
        let fired = Arc::new(Mutex::new(Vec::new()));
        {
            let fired = fired.clone();
            watcher
                .add(
                    Arc::<Path>::from(Path::new("C:\\repo\\src")),
                    WatcherMode::Native,
                    false,
                    move |_| fired.lock().push(()),
                )
                .expect("add watch");
        }

        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("\\\\?\\C:\\repo\\src\\main.rs")),
        );

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

    /// The real-inotify tests share the global watcher and its budget, so they run one at a
    /// time.
    #[cfg(target_os = "linux")]
    static REAL_INOTIFY_TESTS: Mutex<()> = Mutex::new(());

    #[cfg(target_os = "linux")]
    struct GlobalBudgetSourceOverride;

    #[cfg(target_os = "linux")]
    impl GlobalBudgetSourceOverride {
        fn install(source: Arc<dyn WatchBudgetSource>) -> Self {
            global_watcher().budget.set_source(source);
            Self
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for GlobalBudgetSourceOverride {
        fn drop(&mut self) {
            global_watcher()
                .budget
                .set_source(Arc::new(ProcfsBudgetSource));
        }
    }

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

    /// Watches fresh directories through a real `FsWatcher`, checks that a directory left to
    /// the fallback poller reports a new file, and returns how many inotify watches this
    /// process held once every directory was registered.
    #[cfg(target_os = "linux")]
    fn watch_directories_with_real_inotify(
        cx: &gpui::TestAppContext,
        directory_count: usize,
    ) -> usize {
        install_stderr_logger();
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let directories = make_directories(
            temp_dir.path(),
            (0..directory_count).map(|index| format!("dir-{index:04}")),
        );
        let (tx, _rx) = async_channel::unbounded();
        let pending_path_events: Arc<Mutex<Vec<PathEvent>>> = Default::default();
        let watcher = FsWatcher::new(cx.executor(), tx, pending_path_events.clone());
        for directory in &directories {
            watcher.add(directory).expect("watch directory");
        }
        assert!(
            watcher.pending_registrations.lock().is_empty(),
            "no path should be left retrying"
        );
        // Trimming to a learned limit happens on the maintenance thread.
        let deadline = Instant::now() + Duration::from_secs(10);
        while global_watcher()
            .budget
            .learned_limit()
            .is_some_and(|learned_limit| {
                global_watcher().budget.native_watches() > learned_limit / 4
            })
        {
            assert!(
                Instant::now() < deadline,
                "the maintenance thread never trimmed to the learned limit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let own_watches = watch_budget::count_process_inotify_watches(Path::new("/proc/self"));
        {
            let state = global_watcher().state.lock();
            eprintln!(
                "registered {directory_count} directories: {} native, {} polled; budget counts {} native watches, learned limit {:?}; /proc/self lists {own_watches} inotify watches",
                state.native_path_registrations.len(),
                state.fallback_path_registrations.len(),
                global_watcher().budget.native_watches(),
                global_watcher().budget.learned_limit(),
            );
        }

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
                id.and_then(|id| registration_mode(global_watcher(), id))
                    == Some(WatcherMode::Fallback)
            })
            .expect("some directories are left to the fallback poller");
        let file = fallback_directory.join("created.txt");
        fs::write(&file, b"x").expect("create file");
        // The poller runs on its own thread by the wall clock, not on the test executor.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !pending_path_events
            .lock()
            .iter()
            .any(|event| event.path == file)
        {
            assert!(
                Instant::now() < deadline,
                "the fallback poller never reported {file:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(watcher);
        own_watches
    }

    #[cfg(target_os = "linux")]
    #[gpui::test]
    async fn real_inotify_watches_stay_within_an_injected_limit(cx: &mut gpui::TestAppContext) {
        const LIMIT: usize = 400;
        let _serialized = REAL_INOTIFY_TESTS.lock();
        let _override = GlobalBudgetSourceOverride::install(Arc::new(ProcessOnlyBudgetSource {
            limit: Some(LIMIT),
        }));

        let own_watches = watch_directories_with_real_inotify(cx, 300);

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
    async fn real_inotify_limit_is_learned_from_watch_limit_errors(cx: &mut gpui::TestAppContext) {
        let _serialized = REAL_INOTIFY_TESTS.lock();
        let _override =
            GlobalBudgetSourceOverride::install(Arc::new(ProcessOnlyBudgetSource { limit: None }));

        let own_watches = watch_directories_with_real_inotify(cx, 1000);

        let learned_limit = global_watcher()
            .budget
            .learned_limit()
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

pub fn global<T>(f: impl FnOnce(&GlobalWatcher) -> T) -> anyhow::Result<T> {
    let global_watcher = global_watcher();
    global_watcher.ensure_native_watcher()?;
    Ok(f(global_watcher))
}
