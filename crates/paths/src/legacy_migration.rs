//! Moving this application's directories out of the places an earlier build
//! of this fork kept them under its old name ([`branding::LEGACY`]).
//!
//! The move runs once per launch, before any directory is resolved. A legacy
//! directory is renamed only when nothing can still be using it; otherwise this
//! launch keeps resolving that directory under the old name, so that no data
//! is ever copied, split between two places or lost.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use branding::Names;

/// The directories this crate derives from the application's directory name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryKind {
    Config,
    Data,
    State,
    Temp,
    /// Only macOS keeps its logs outside the data directory.
    Logs,
}

impl DirectoryKind {
    const ALL: [DirectoryKind; 5] = [
        DirectoryKind::Config,
        DirectoryKind::Data,
        DirectoryKind::State,
        DirectoryKind::Temp,
        DirectoryKind::Logs,
    ];

    fn index(self) -> usize {
        match self {
            DirectoryKind::Config => 0,
            DirectoryKind::Data => 1,
            DirectoryKind::State => 2,
            DirectoryKind::Temp => 3,
            DirectoryKind::Logs => 4,
        }
    }
}

/// Which set of names each kind of directory is resolved under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedNames([&'static Names; DirectoryKind::ALL.len()]);

impl ResolvedNames {
    fn current() -> Self {
        Self([&branding::CURRENT; DirectoryKind::ALL.len()])
    }

    pub(crate) fn names(&self, kind: DirectoryKind) -> &'static Names {
        self.0[kind.index()]
    }

    fn set(&mut self, kinds: &[DirectoryKind], names: &'static Names) {
        for kind in kinds {
            self.0[kind.index()] = names;
        }
    }
}

static RESOLVED_NAMES: OnceLock<ResolvedNames> = OnceLock::new();

/// The set of names `kind` is resolved under in this process: the current
/// names, unless the migration had to leave that directory under an old name.
pub(crate) fn resolved_names(kind: DirectoryKind) -> &'static Names {
    RESOLVED_NAMES
        .get_or_init(ResolvedNames::current)
        .names(kind)
}

/// One place an application directory is created in, such as
/// `%APPDATA%\<name>` or `$XDG_STATE_HOME/<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryLocation {
    /// Every kind resolved to this place. On Windows the data, state and temp
    /// directories are one and the same.
    pub kinds: Vec<DirectoryKind>,
    pub parent: PathBuf,
    /// Whether the directory is named with the lowercase form of the name.
    pub lowercase: bool,
    /// Where inside the directory the log files are, if they are in it.
    pub logs_subdirectory: Option<PathBuf>,
}

impl DirectoryLocation {
    fn directory(&self, names: &Names) -> PathBuf {
        self.parent.join(if self.lowercase {
            names.app_directory_name_lowercase
        } else {
            names.app_directory_name
        })
    }
}

/// The result of [`migrate_legacy_app_dirs`] or
/// [`migrate_legacy_remote_server_dirs`].
#[derive(Debug)]
pub enum LegacyMigration {
    /// `--user-data-dir` is in effect, so nothing was looked at.
    CustomDataDirectory,
    /// Some directory had already been resolved, so moving anything could have
    /// pulled it out from under whatever resolved it.
    DirectoriesAlreadyResolved,
    /// One entry per legacy directory that exists.
    Checked(Vec<DirectoryMigration>),
}

#[derive(Debug)]
pub struct DirectoryMigration {
    pub legacy: PathBuf,
    /// The application directory name `legacy` is named with.
    pub legacy_name: &'static str,
    pub current: PathBuf,
    pub outcome: MigrationOutcome,
}

/// Put in a directory moved out of its legacy name, so that a legacy directory
/// found next to it later is known to have been created again by an earlier
/// build. It holds the identity of the last such directory the user was told
/// about.
pub(crate) const LEGACY_DIRECTORY_NOTICE_FILE_NAME: &str = "legacy-directory-notice";

#[derive(Debug)]
pub enum MigrationOutcome {
    /// The legacy directory now has the current name. Log files that could
    /// not be renamed to the current name are listed.
    Moved {
        log_rename_errors: Vec<(PathBuf, io::Error)>,
        /// Set when another directory could not be moved and this one could
        /// not be moved back either.
        move_back_error: Option<io::Error>,
        /// Set when the notice file could not be written into the moved
        /// directory, so a legacy directory created again later goes untold.
        notice_file_error: Option<io::Error>,
    },
    /// The current directory holds something, so it is used and the legacy
    /// one is left alone.
    CurrentAlreadyExists,
    /// Like [`MigrationOutcome::CurrentAlreadyExists`], but the legacy
    /// directory was created again after it had been moved (an earlier build
    /// was launched since), and the user has not been told about this one
    /// yet. Whatever that build changed in it is not used.
    LegacyCreatedAgain {
        /// Set when recording that the user was told failed, so they may be
        /// told again on the next launch.
        notice_file_error: Option<io::Error>,
    },
    /// The legacy directory was moved, then moved back because another one
    /// could not be moved, so that this launch does not find its settings
    /// under one name and its data under the other. It keeps using the
    /// legacy directory.
    MovedBack,
    /// A build that may be using the legacy directory is running, so this
    /// launch keeps using the legacy directory.
    LegacyBuildRunning,
    /// The legacy directory could not be renamed, so this launch keeps using
    /// it.
    MoveFailed(io::Error),
}

impl LegacyMigration {
    /// The legacy directories this process keeps using instead of the current
    /// ones.
    pub fn legacy_directories_in_use(&self) -> Vec<&DirectoryMigration> {
        match self {
            LegacyMigration::Checked(directories) => directories
                .iter()
                .filter(|directory| {
                    matches!(
                        directory.outcome,
                        MigrationOutcome::LegacyBuildRunning
                            | MigrationOutcome::MoveFailed(_)
                            | MigrationOutcome::MovedBack
                    )
                })
                .collect(),
            LegacyMigration::CustomDataDirectory | LegacyMigration::DirectoriesAlreadyResolved => {
                Vec::new()
            }
        }
    }

    /// The legacy directories an earlier build created again after they had
    /// been moved, which the user has not been told about yet.
    pub fn legacy_directories_created_again(&self) -> Vec<&DirectoryMigration> {
        match self {
            LegacyMigration::Checked(directories) => directories
                .iter()
                .filter(|directory| {
                    matches!(
                        directory.outcome,
                        MigrationOutcome::LegacyCreatedAgain { .. }
                    )
                })
                .collect(),
            LegacyMigration::CustomDataDirectory | LegacyMigration::DirectoriesAlreadyResolved => {
                Vec::new()
            }
        }
    }

    /// Whether a directory is kept under its legacy name because a build that
    /// may be using it is running (rather than because it failed to move).
    pub fn legacy_build_is_running(&self) -> bool {
        self.legacy_directories_in_use()
            .iter()
            .any(|directory| matches!(directory.outcome, MigrationOutcome::LegacyBuildRunning))
    }

    /// Logs what happened. The migration runs before logging is set up, so the
    /// caller does this once it is.
    pub fn log(&self) {
        let directories = match self {
            LegacyMigration::CustomDataDirectory => return,
            LegacyMigration::DirectoriesAlreadyResolved => {
                log::error!(
                    "the application directories were resolved before they could be moved \
                     out of their legacy names"
                );
                return;
            }
            LegacyMigration::Checked(directories) => directories,
        };
        for directory in directories {
            let legacy = &directory.legacy;
            let current = &directory.current;
            match &directory.outcome {
                MigrationOutcome::Moved {
                    log_rename_errors,
                    move_back_error,
                    notice_file_error,
                } => {
                    log::info!("moved {legacy:?} to {current:?}");
                    for (log_file, error) in log_rename_errors {
                        log::warn!("could not rename the log file {log_file:?}: {error}");
                    }
                    if let Some(error) = move_back_error {
                        log::warn!(
                            "{current:?} could not be moved back after another directory \
                             failed to move: {error}"
                        );
                    }
                    if let Some(error) = notice_file_error {
                        log::warn!("could not write the notice file into {current:?}: {error}");
                    }
                }
                MigrationOutcome::CurrentAlreadyExists => {
                    log::info!("{legacy:?} is left in place because {current:?} already exists");
                }
                MigrationOutcome::LegacyCreatedAgain { notice_file_error } => {
                    log::warn!(
                        "{legacy:?} was created again after it had been moved to {current:?}; \
                         it is not used"
                    );
                    if let Some(error) = notice_file_error {
                        log::warn!("could not write the notice file into {current:?}: {error}");
                    }
                }
                MigrationOutcome::MovedBack => {
                    log::warn!(
                        "using {legacy:?} instead of {current:?}: it was moved back because \
                         another directory could not be moved"
                    );
                }
                MigrationOutcome::LegacyBuildRunning => {
                    log::warn!(
                        "using {legacy:?} instead of {current:?}: a build that may be using it \
                         is running"
                    );
                }
                MigrationOutcome::MoveFailed(error) => {
                    log::warn!(
                        "using {legacy:?} instead of {current:?}: it could not be moved: {error}"
                    );
                }
            }
        }
    }
}

/// The existing directories of one legacy set of names, for deciding whether a
/// build that uses them is running.
pub(crate) struct LegacyDirectories<'a> {
    /// Only Windows finds a running build by name; elsewhere it is found in
    /// its directories.
    #[cfg_attr(not(any(test, target_os = "windows")), allow(dead_code))]
    pub names: &'static Names,
    pub directories: &'a [PathBuf],
}

pub(crate) struct Migration<'a> {
    pub locations: &'a [DirectoryLocation],
    pub legacy_sets: &'static [Names],
    pub custom_data_dir_is_set: bool,
    pub legacy_build_is_running: &'a dyn Fn(&LegacyDirectories) -> bool,
    pub rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
}

impl Migration<'_> {
    pub(crate) fn run(&self) -> (LegacyMigration, ResolvedNames) {
        let mut resolved = ResolvedNames::current();
        if self.custom_data_dir_is_set {
            return (LegacyMigration::CustomDataDirectory, resolved);
        }

        let current_names = &branding::CURRENT;
        let mut running = vec![None; self.legacy_sets.len()];
        let mut directories = Vec::new();
        let mut moved: Vec<(usize, &DirectoryLocation, &'static Names)> = Vec::new();
        let mut a_move_failed = false;
        for location in self.locations {
            // A relative parent (an empty environment variable) would resolve
            // against whatever the working directory happens to be.
            if !location.parent.is_absolute() {
                continue;
            }
            let current = location.directory(current_names);
            for (legacy_index, legacy_names) in self.legacy_sets.iter().enumerate() {
                let legacy = location.directory(legacy_names);
                if legacy == current || !exists(&legacy) {
                    continue;
                }
                let outcome = if holds_anything(&current) {
                    legacy_created_again_outcome(&legacy, &current)
                } else if *running[legacy_index]
                    .get_or_insert_with(|| self.legacy_set_is_in_use(legacy_names))
                {
                    resolved.set(&location.kinds, legacy_names);
                    MigrationOutcome::LegacyBuildRunning
                } else if !remove_empty_directory_tree(&current) {
                    MigrationOutcome::CurrentAlreadyExists
                } else {
                    match (self.rename)(&legacy, &current) {
                        Ok(()) => {
                            moved.push((directories.len(), location, legacy_names));
                            MigrationOutcome::Moved {
                                log_rename_errors: Vec::new(),
                                move_back_error: None,
                                notice_file_error: None,
                            }
                        }
                        // Another process of this build launched at the same
                        // moment moved it first.
                        Err(_) if exists(&current) && !exists(&legacy) => {
                            MigrationOutcome::CurrentAlreadyExists
                        }
                        Err(error) => {
                            a_move_failed = true;
                            resolved.set(&location.kinds, legacy_names);
                            MigrationOutcome::MoveFailed(error)
                        }
                    }
                };
                directories.push(DirectoryMigration {
                    legacy,
                    legacy_name: legacy_names.app_directory_name,
                    current,
                    outcome,
                });
                // The newest legacy directory there is wins; older ones are
                // not merged into it.
                break;
            }
        }

        // A build this one cannot detect (one that takes no single-instance
        // lock) keeps files open in its data directory but not in its
        // configuration directory, so only the latter moves. Moving it back
        // keeps that build's later settings from landing in a recreated
        // legacy directory that this build would then ignore.
        for (index, location, legacy_names) in moved.into_iter().rev() {
            let Some(directory) = directories.get_mut(index) else {
                continue;
            };
            let mut move_back_error = None;
            if a_move_failed {
                match (self.rename)(&directory.current, &directory.legacy) {
                    Ok(()) => {
                        resolved.set(&location.kinds, legacy_names);
                        directory.outcome = MigrationOutcome::MovedBack;
                        continue;
                    }
                    Err(error) => move_back_error = Some(error),
                }
            }
            directory.outcome = MigrationOutcome::Moved {
                log_rename_errors: self.rename_log_files(
                    location,
                    &directory.current,
                    legacy_names,
                    current_names,
                ),
                move_back_error,
                notice_file_error: std::fs::write(
                    directory.current.join(LEGACY_DIRECTORY_NOTICE_FILE_NAME),
                    "",
                )
                .err(),
            };
        }
        (LegacyMigration::Checked(directories), resolved)
    }

    fn legacy_set_is_in_use(&self, names: &'static Names) -> bool {
        let directories: Vec<PathBuf> = self
            .locations
            .iter()
            .map(|location| location.directory(names))
            .filter(|directory| exists(directory))
            .collect();
        (self.legacy_build_is_running)(&LegacyDirectories {
            names,
            directories: &directories,
        })
    }

    fn rename_log_files(
        &self,
        location: &DirectoryLocation,
        moved_directory: &Path,
        legacy_names: &Names,
        current_names: &Names,
    ) -> Vec<(PathBuf, io::Error)> {
        let Some(logs_subdirectory) = &location.logs_subdirectory else {
            return Vec::new();
        };
        let logs_directory = moved_directory.join(logs_subdirectory);
        let mut errors = Vec::new();
        for suffix in [".log", ".log.old"] {
            let legacy_log =
                logs_directory.join(format!("{}{suffix}", legacy_names.app_directory_name));
            let current_log =
                logs_directory.join(format!("{}{suffix}", current_names.app_directory_name));
            if !exists(&legacy_log) || exists(&current_log) {
                continue;
            }
            if let Err(error) = (self.rename)(&legacy_log, &current_log) {
                errors.push((legacy_log, error));
            }
        }
        errors
    }
}

/// Whether `path` exists, taking a path that cannot be checked as existing so
/// that nothing is moved onto or out of it.
fn exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(true)
}

/// More directories than an installer or an aborted launch leaves behind.
const EMPTY_TREE_DIRECTORY_LIMIT: usize = 256;

/// The directories of the tree at `path`, parents before children, if the
/// tree is real directories only. Anything else (a file, a link, a junction,
/// something unreadable, or too many directories) makes it `None`.
fn empty_directory_tree(path: &Path) -> Option<Vec<PathBuf>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    let mut directories = vec![path.to_path_buf()];
    let mut next = 0;
    while let Some(directory) = directories.get(next).cloned() {
        next += 1;
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            // `FileType::is_dir` is false for symbolic links and junctions.
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            if directories.len() >= EMPTY_TREE_DIRECTORY_LIMIT {
                return None;
            }
            directories.push(entry.path());
        }
    }
    Some(directories)
}

/// Whether `path` exists and is something other than empty directories, such
/// as the ones an installer or a launch with `--user-data-dir` creates.
fn holds_anything(path: &Path) -> bool {
    exists(path) && empty_directory_tree(path).is_none()
}

/// Removes `path` if it is empty directories only, so that the legacy
/// directory can take its place. Only `remove_dir` is used, which refuses a
/// directory something was put into meanwhile. Returns whether `path` is gone.
fn remove_empty_directory_tree(path: &Path) -> bool {
    if !exists(path) {
        return true;
    }
    let Some(directories) = empty_directory_tree(path) else {
        return false;
    };
    directories
        .iter()
        .rev()
        .all(|directory| std::fs::remove_dir(directory).is_ok())
}

/// The outcome for a legacy directory found next to a current one that holds
/// something: [`MigrationOutcome::LegacyCreatedAgain`] once per legacy
/// directory created again after the move, and
/// [`MigrationOutcome::CurrentAlreadyExists`] otherwise. Nothing here stops
/// the launch: a notice file that cannot be read means no notice.
fn legacy_created_again_outcome(legacy: &Path, current: &Path) -> MigrationOutcome {
    let notice_file = current.join(LEGACY_DIRECTORY_NOTICE_FILE_NAME);
    // Without the notice file, the current directory was not moved here by
    // this build, so the legacy one may be what the user chose to keep.
    let Ok(told_identity) = std::fs::read_to_string(&notice_file) else {
        return MigrationOutcome::CurrentAlreadyExists;
    };
    // Empty directories hold no change the user could miss.
    if !holds_anything(legacy) {
        return MigrationOutcome::CurrentAlreadyExists;
    }
    // A directory whose identity cannot be read is told about once.
    let identity = directory_identity(legacy).unwrap_or_else(|| "unknown".to_string());
    if told_identity == identity {
        return MigrationOutcome::CurrentAlreadyExists;
    }
    MigrationOutcome::LegacyCreatedAgain {
        notice_file_error: std::fs::write(&notice_file, identity).err(),
    }
}

/// What tells one directory at `path` from another created there later: its
/// file system identity and its creation time.
fn directory_identity(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let mut parts = Vec::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        parts.push(format!("{}:{}", metadata.dev(), metadata.ino()));
    }
    #[cfg(target_os = "windows")]
    if let Some(file_index) = platform::windows_file_index(path) {
        parts.push(file_index);
    }
    if let Ok(created) = metadata.created()
        && let Ok(since_epoch) = created.duration_since(std::time::UNIX_EPOCH)
    {
        parts.push(since_epoch.as_nanos().to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Which names each kind of directory resolves under without moving anything:
/// a legacy directory is used where it exists and the current one holds
/// nothing. This is what a launch that runs the migration ends up with
/// whenever it leaves a legacy directory in use, so a helper process resolves
/// the same directories as the instance it serves.
fn resolve_without_moving(
    locations: &[DirectoryLocation],
    legacy_sets: &'static [Names],
) -> ResolvedNames {
    let mut resolved = ResolvedNames::current();
    for location in locations {
        if !location.parent.is_absolute() {
            continue;
        }
        let current = location.directory(&branding::CURRENT);
        if holds_anything(&current) {
            continue;
        }
        if let Some(legacy_names) = legacy_sets.iter().find(|names| {
            let legacy = location.directory(names);
            legacy != current && exists(&legacy)
        }) {
            resolved.set(&location.kinds, legacy_names);
        }
    }
    resolved
}

/// Moves this application's directories out of their legacy names (see the
/// module documentation). Call it before anything resolves a directory; a
/// launch with `--user-data-dir` must not call it.
///
/// A legacy directory is treated as in use while an instance of an earlier
/// build, or of this one, that may be running from it is found.
pub fn migrate_legacy_app_dirs() -> LegacyMigration {
    migrate(&platform::application_instance_is_running)
}

/// [`migrate_legacy_app_dirs`] for the remote server, which treats a legacy
/// directory as in use while a pid file in its server state names a process
/// `pid_is_alive` reports alive.
pub fn migrate_legacy_remote_server_dirs(pid_is_alive: &dyn Fn(u32) -> bool) -> LegacyMigration {
    migrate(&|legacy: &LegacyDirectories| {
        remote_server_is_running(legacy.directories, pid_is_alive)
    })
}

/// Resolves this application's directories the way the instance a helper
/// process serves resolved them, without moving anything. Call it instead of
/// [`migrate_legacy_app_dirs`] in a helper process (the crash handler,
/// askpass, a remote server daemon) and in a launch with `--user-data-dir`,
/// whose state and temporary directories are still the real ones.
pub fn resolve_app_dirs_without_migrating() {
    RESOLVED_NAMES.get_or_init(|| resolve_without_moving(&platform::locations(), branding::LEGACY));
}

fn migrate(legacy_build_is_running: &dyn Fn(&LegacyDirectories) -> bool) -> LegacyMigration {
    let mut migration = None;
    RESOLVED_NAMES.get_or_init(|| {
        let locations = platform::locations();
        let (result, resolved) = Migration {
            locations: &locations,
            legacy_sets: branding::LEGACY,
            custom_data_dir_is_set: crate::CUSTOM_DATA_DIR.get().is_some(),
            legacy_build_is_running,
            rename: &|from, to| std::fs::rename(from, to),
        }
        .run();
        migration = Some(result);
        resolved
    });
    migration.unwrap_or(LegacyMigration::DirectoriesAlreadyResolved)
}

const REMOTE_SERVER_PID_FILE_NAMES: [&str; 2] = ["daemon.pid", "server.pid"];

fn remote_server_is_running(directories: &[PathBuf], pid_is_alive: &dyn Fn(u32) -> bool) -> bool {
    for directory in directories {
        let Ok(state_directories) = std::fs::read_dir(directory.join("server_state")) else {
            continue;
        };
        for state_directory in state_directories.flatten() {
            for pid_file_name in REMOTE_SERVER_PID_FILE_NAMES {
                let pid_file = state_directory.path().join(pid_file_name);
                let Ok(contents) = std::fs::read_to_string(&pid_file) else {
                    continue;
                };
                if let Ok(pid) = contents.trim().parse::<u32>()
                    && pid_is_alive(pid)
                {
                    return true;
                }
            }
        }
    }
    false
}

mod platform {
    use super::{DirectoryKind, DirectoryLocation, LegacyDirectories};
    use std::path::PathBuf;

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn flatpak_or(variable: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
        std::env::var(variable).ok().map(PathBuf::from).or(fallback)
    }

    /// Where this platform keeps each kind of directory; this mirrors
    /// `config_dir`, `data_dir`, `state_dir`, `temp_dir` and `logs_dir`.
    pub(super) fn locations() -> Vec<DirectoryLocation> {
        let mut locations: Vec<DirectoryLocation> = Vec::new();
        let mut add = |kind: DirectoryKind,
                       parent: Option<PathBuf>,
                       lowercase: bool,
                       logs_subdirectory: Option<&str>| {
            let Some(parent) = parent else {
                return;
            };
            let logs_subdirectory = logs_subdirectory.map(PathBuf::from);
            if let Some(location) = locations
                .iter_mut()
                .find(|location| location.parent == parent && location.lowercase == lowercase)
            {
                location.kinds.push(kind);
                if location.logs_subdirectory.is_none() {
                    location.logs_subdirectory = logs_subdirectory;
                }
            } else {
                locations.push(DirectoryLocation {
                    kinds: vec![kind],
                    parent,
                    lowercase,
                    logs_subdirectory,
                });
            }
        };
        #[cfg(target_os = "windows")]
        {
            // The data directory first: a running build keeps files open in
            // it, which makes its move fail before the configuration
            // directory, which nothing keeps open, is moved at all.
            add(
                DirectoryKind::Data,
                dirs::data_local_dir(),
                false,
                Some("logs"),
            );
            add(DirectoryKind::State, dirs::data_local_dir(), false, None);
            add(DirectoryKind::Temp, dirs::cache_dir(), false, None);
            add(DirectoryKind::Config, dirs::config_dir(), false, None);
        }
        #[cfg(target_os = "macos")]
        {
            let home = crate::home_dir();
            add(
                DirectoryKind::Config,
                Some(home.join(".config")),
                true,
                None,
            );
            add(
                DirectoryKind::Data,
                Some(home.join("Library/Application Support")),
                false,
                None,
            );
            add(
                DirectoryKind::State,
                Some(home.join(".local").join("state")),
                false,
                None,
            );
            add(DirectoryKind::Temp, dirs::cache_dir(), false, None);
            add(
                DirectoryKind::Logs,
                Some(home.join("Library/Logs")),
                false,
                Some(""),
            );
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            add(
                DirectoryKind::Config,
                flatpak_or("FLATPAK_XDG_CONFIG_HOME", dirs::config_dir()),
                true,
                None,
            );
            add(
                DirectoryKind::Data,
                flatpak_or("FLATPAK_XDG_DATA_HOME", dirs::data_local_dir()),
                true,
                Some("logs"),
            );
            add(
                DirectoryKind::State,
                flatpak_or("FLATPAK_XDG_STATE_HOME", dirs::state_dir()),
                true,
                None,
            );
            add(
                DirectoryKind::Temp,
                flatpak_or("FLATPAK_XDG_CACHE_HOME", dirs::cache_dir()),
                true,
                None,
            );
        }
        #[cfg(not(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "freebsd"
        )))]
        {
            // `data_dir` falls back to `config_dir` here.
            let home = crate::home_dir();
            add(
                DirectoryKind::Config,
                Some(home.join(".config")),
                true,
                Some("logs"),
            );
            add(DirectoryKind::State, dirs::data_local_dir(), false, None);
            add(DirectoryKind::Temp, Some(home.join(".cache")), true, None);
        }
        locations
    }

    /// Whether an instance that may be running from `legacy`'s directories
    /// is running: one of an earlier build, or one of this build that started
    /// while an earlier build was running and so kept the legacy directories.
    #[cfg_attr(
        not(any(target_os = "windows", target_os = "linux", target_os = "freebsd")),
        allow(unused_variables)
    )]
    pub(super) fn application_instance_is_running(legacy: &LegacyDirectories) -> bool {
        #[cfg(target_os = "windows")]
        {
            let identifiers = legacy
                .names
                .app_identifiers
                .all()
                .into_iter()
                .chain(branding::CURRENT.app_identifiers.all());
            windows_instance_mutex_exists(identifiers)
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            linux_instance_socket_is_listening(legacy.directories)
        }
        #[cfg(target_os = "macos")]
        {
            // The builds before the rename answer on an official Zed's port
            // and handshake, so this also finds an official Zed and then
            // defers the move until it quits.
            macos_instance_answers_handshake()
        }
        #[cfg(not(any(
            target_os = "windows",
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos"
        )))]
        {
            false
        }
    }

    /// Whether a single-instance mutex (see `windows_only_instance.rs` in the
    /// `zed` crate) exists for one of `identifiers`.
    #[cfg(target_os = "windows")]
    pub(super) fn windows_instance_mutex_exists<'a>(
        identifiers: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        use util::ResultExt as _;
        use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND};
        use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
        use windows::core::HSTRING;

        for identifier in identifiers {
            let name = HSTRING::from(format!("{identifier}-Instance-Mutex"));
            // SAFETY: `name` is a valid wide string that outlives the call.
            match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, &name) } {
                Ok(handle) => {
                    // SAFETY: `handle` was just opened and is closed once.
                    unsafe { CloseHandle(handle) }.log_err();
                    return true;
                }
                Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {}
                // Any other failure, such as an elevated instance's mutex
                // denying access, still means the mutex exists.
                Err(_) => return true,
            }
        }
        false
    }

    /// The volume serial number and file index of the directory at `path`,
    /// which a directory created again in its place does not share.
    #[cfg(target_os = "windows")]
    pub(super) fn windows_file_index(path: &std::path::Path) -> Option<String> {
        use std::os::windows::fs::OpenOptionsExt as _;
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
            GetFileInformationByHandle,
        };

        // Opening a directory needs the backup semantics flag.
        let directory = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
            .ok()?;
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle stays open for the call and `information` is a
        // valid out pointer.
        unsafe { GetFileInformationByHandle(HANDLE(directory.as_raw_handle()), &mut information) }
            .ok()?;
        Some(format!(
            "{:x}:{:x}{:08x}",
            information.dwVolumeSerialNumber, information.nFileIndexHigh, information.nFileIndexLow
        ))
    }

    /// Whether an instance listens on the socket the `zed` crate's
    /// `listen_for_cli_connections` binds in its data directory.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) fn linux_instance_socket_is_listening(directories: &[PathBuf]) -> bool {
        use std::os::unix::net::UnixDatagram;

        const CHANNEL_NAMES: [&str; 4] = ["dev", "nightly", "preview", "stable"];
        let Ok(socket) = UnixDatagram::unbound() else {
            return true;
        };
        directories.iter().any(|directory| {
            CHANNEL_NAMES.iter().any(|channel| {
                socket
                    .connect(directory.join(format!("zed-{channel}.sock")))
                    .is_ok()
            })
        })
    }

    /// The loopback ports and handshakes the `zed` crate's
    /// `mac_only_instance.rs` answers on for `user_id`, in every release
    /// channel: those of the builds before the rename, and those of this
    /// build, which keeps the legacy directories when it starts while an
    /// earlier build is running.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(super) fn macos_instance_probes(user_id: u32) -> Vec<(u16, String)> {
        let legacy = branding::LEGACY_MAC_INSTANCE_PORTS
            .ports(Some(user_id))
            .all()
            .into_iter()
            .zip(branding::LEGACY_MAC_INSTANCE_HANDSHAKES.all())
            .map(|(port, handshake)| (port, handshake.to_string()));
        let current = branding::MAC_INSTANCE_PORTS
            .ports(Some(user_id))
            .all()
            .into_iter()
            .zip(branding::CURRENT.app_identifiers.all())
            .map(|(port, app_identifier)| (port, branding::mac_instance_handshake(app_identifier)));
        legacy.chain(current).collect()
    }

    #[cfg(target_os = "macos")]
    fn macos_instance_answers_handshake() -> bool {
        use std::io::Read as _;
        use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
        use std::time::Duration;

        // SAFETY: `getuid` has no preconditions and cannot fail.
        let user_id = unsafe { libc::getuid() };
        macos_instance_probes(user_id)
            .iter()
            .any(|(port, handshake)| {
                let address = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, *port));
                let Ok(mut stream) =
                    TcpStream::connect_timeout(&address, Duration::from_millis(10))
                else {
                    return false;
                };
                if stream
                    .set_read_timeout(Some(Duration::from_millis(35)))
                    .is_err()
                {
                    return false;
                }
                let mut received = vec![0u8; handshake.len()];
                stream.read_exact(&mut received).is_ok() && received == handshake.as_bytes()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;

    const LEGACY: &Names = &branding::ZED_JP_ENHANCE;
    const CURRENT: &Names = &branding::CURRENT;

    struct Fixture {
        _root: tempfile::TempDir,
        roaming: PathBuf,
        local: PathBuf,
        locations: Vec<DirectoryLocation>,
    }

    /// A Windows-shaped layout (a roaming config directory, and one local
    /// directory for data, state and temp) inside a temporary directory.
    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("temporary directory");
        let roaming = root.path().join("Roaming");
        let local = root.path().join("Local");
        fs::create_dir_all(&roaming).expect("roaming");
        fs::create_dir_all(&local).expect("local");
        let locations = vec![
            DirectoryLocation {
                kinds: vec![DirectoryKind::Config],
                parent: roaming.clone(),
                lowercase: false,
                logs_subdirectory: None,
            },
            DirectoryLocation {
                kinds: vec![
                    DirectoryKind::Data,
                    DirectoryKind::State,
                    DirectoryKind::Temp,
                ],
                parent: local.clone(),
                lowercase: false,
                logs_subdirectory: Some(PathBuf::from("logs")),
            },
        ];
        Fixture {
            _root: root,
            roaming,
            local,
            locations,
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        fs::write(path, contents).expect("write");
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_else(|error| panic!("reading {path:?}: {error}"))
    }

    fn populate_legacy(fixture: &Fixture) {
        let config = fixture.roaming.join(LEGACY.app_directory_name);
        write(&config.join("settings.json"), "{\"legacy\": true}");
        write(&config.join("network_allowlist.json"), "[\"legacy\"]");
        let data = fixture.local.join(LEGACY.app_directory_name);
        write(&data.join("db/0-stable/db.sqlite"), "legacy database");
        write(
            &data
                .join("logs")
                .join(format!("{}.log", LEGACY.app_directory_name)),
            "legacy log",
        );
        write(
            &data
                .join("logs")
                .join(format!("{}.log.old", LEGACY.app_directory_name)),
            "legacy old log",
        );
    }

    fn run_with(
        fixture: &Fixture,
        custom_data_dir_is_set: bool,
        legacy_build_is_running: &dyn Fn(&LegacyDirectories) -> bool,
        rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
    ) -> (LegacyMigration, ResolvedNames) {
        Migration {
            locations: &fixture.locations,
            legacy_sets: branding::LEGACY,
            custom_data_dir_is_set,
            legacy_build_is_running,
            rename,
        }
        .run()
    }

    fn run(fixture: &Fixture) -> (LegacyMigration, ResolvedNames) {
        run_with(fixture, false, &|_| false, &|from, to| fs::rename(from, to))
    }

    fn outcomes(migration: &LegacyMigration) -> &[DirectoryMigration] {
        match migration {
            LegacyMigration::Checked(directories) => directories,
            other => panic!("expected the directories to be checked, got {other:?}"),
        }
    }

    fn assert_resolved_everywhere(resolved: &ResolvedNames, names: &Names) {
        for kind in [
            DirectoryKind::Config,
            DirectoryKind::Data,
            DirectoryKind::State,
            DirectoryKind::Temp,
        ] {
            assert_eq!(resolved.names(kind), names, "{kind:?}");
        }
    }

    #[test]
    fn the_migration_has_something_to_do() {
        assert_ne!(
            LEGACY.app_directory_name, CURRENT.app_directory_name,
            "these tests assume the current names differ from the legacy ones"
        );
        assert_ne!(
            LEGACY.app_directory_name_lowercase,
            CURRENT.app_directory_name_lowercase
        );
    }

    #[test]
    fn only_legacy_directories_are_moved_with_their_contents() {
        let fixture = fixture();
        populate_legacy(&fixture);

        let (migration, resolved) = run(&fixture);

        assert_eq!(outcomes(&migration).len(), 2);
        for directory in outcomes(&migration) {
            assert!(
                matches!(
                    &directory.outcome,
                    MigrationOutcome::Moved {
                        log_rename_errors,
                        move_back_error: None,
                        notice_file_error: None,
                    } if log_rename_errors.is_empty()
                ),
                "{directory:?}"
            );
        }
        assert!(migration.legacy_directories_in_use().is_empty());
        assert_resolved_everywhere(&resolved, CURRENT);

        assert!(!fixture.roaming.join(LEGACY.app_directory_name).exists());
        assert!(!fixture.local.join(LEGACY.app_directory_name).exists());
        let config = fixture.roaming.join(CURRENT.app_directory_name);
        assert_eq!(read(&config.join("settings.json")), "{\"legacy\": true}");
        assert_eq!(read(&config.join("network_allowlist.json")), "[\"legacy\"]");
        let data = fixture.local.join(CURRENT.app_directory_name);
        assert_eq!(read(&data.join("db/0-stable/db.sqlite")), "legacy database");
        let logs = data.join("logs");
        assert_eq!(
            read(&logs.join(format!("{}.log", CURRENT.app_directory_name))),
            "legacy log"
        );
        assert_eq!(
            read(&logs.join(format!("{}.log.old", CURRENT.app_directory_name))),
            "legacy old log"
        );
        assert!(
            !logs
                .join(format!("{}.log", LEGACY.app_directory_name))
                .exists()
        );
    }

    #[test]
    fn an_existing_current_directory_wins_and_neither_is_changed() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let current_config = fixture.roaming.join(CURRENT.app_directory_name);
        write(&current_config.join("settings.json"), "{\"current\": true}");

        let (migration, resolved) = run(&fixture);

        let directories = outcomes(&migration);
        assert!(
            matches!(
                directories[0].outcome,
                MigrationOutcome::CurrentAlreadyExists
            ),
            "{directories:?}"
        );
        assert_eq!(
            read(&current_config.join("settings.json")),
            "{\"current\": true}"
        );
        assert_eq!(
            read(
                &fixture
                    .roaming
                    .join(LEGACY.app_directory_name)
                    .join("settings.json")
            ),
            "{\"legacy\": true}"
        );
        assert!(
            !current_config.join("network_allowlist.json").exists(),
            "nothing is merged into the current directory"
        );
        assert_resolved_everywhere(&resolved, CURRENT);
        assert!(
            migration.legacy_directories_created_again().is_empty(),
            "a current directory this build did not move there tells nothing"
        );
    }

    fn notified(migration: &LegacyMigration) -> Vec<PathBuf> {
        migration
            .legacy_directories_created_again()
            .into_iter()
            .map(|directory| directory.legacy.clone())
            .collect()
    }

    #[test]
    fn a_legacy_directory_created_again_after_the_move_is_told_about_once() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let roaming_legacy = fixture.roaming.join(LEGACY.app_directory_name);
        let local_legacy = fixture.local.join(LEGACY.app_directory_name);
        let (migration, _) = run(&fixture);
        assert!(notified(&migration).is_empty());

        // An earlier build launched after the move: it creates the
        // configuration directory empty, then writes settings into it.
        fs::create_dir_all(&roaming_legacy).expect("recreate roaming");
        let (migration, _) = run(&fixture);
        assert!(
            notified(&migration).is_empty(),
            "empty directories hold nothing to miss"
        );
        write(&roaming_legacy.join("settings.json"), "{\"again\": true}");
        write(&local_legacy.join("logs/again.log"), "again");

        let (migration, resolved) = run_with(&fixture, false, &|_| true, &|from, to| {
            panic!("nothing may be moved, but {from:?} was moved to {to:?}")
        });
        assert_eq!(
            notified(&migration),
            vec![roaming_legacy.clone(), local_legacy]
        );
        assert!(
            outcomes(&migration)
                .iter()
                .all(|directory| directory.legacy_name == LEGACY.app_directory_name)
        );
        assert_resolved_everywhere(&resolved, CURRENT);
        assert!(migration.legacy_directories_in_use().is_empty());
        assert_eq!(
            read(&roaming_legacy.join("settings.json")),
            "{\"again\": true}"
        );

        let (migration, _) = run(&fixture);
        assert!(
            notified(&migration).is_empty(),
            "the same directories are told about once"
        );

        // Removed and created again: a different directory, told about again.
        fs::remove_dir_all(&roaming_legacy).expect("remove roaming legacy");
        write(&roaming_legacy.join("settings.json"), "{\"third\": true}");
        let (migration, _) = run(&fixture);
        assert_eq!(notified(&migration), vec![roaming_legacy]);
        let (migration, _) = run(&fixture);
        assert!(notified(&migration).is_empty());
    }

    #[test]
    fn a_missing_notice_file_tells_nothing_and_breaks_nothing() {
        let fixture = fixture();
        populate_legacy(&fixture);
        run(&fixture);
        let local_notice = fixture
            .local
            .join(CURRENT.app_directory_name)
            .join(LEGACY_DIRECTORY_NOTICE_FILE_NAME);
        let roaming_notice = fixture
            .roaming
            .join(CURRENT.app_directory_name)
            .join(LEGACY_DIRECTORY_NOTICE_FILE_NAME);
        assert!(
            local_notice.is_file(),
            "a moved directory gets the notice file"
        );
        fs::remove_file(&local_notice).expect("remove the local notice file");
        // One that cannot be read as a file.
        fs::remove_file(&roaming_notice).expect("remove the roaming notice file");
        fs::create_dir(&roaming_notice).expect("a directory in its place");
        populate_legacy(&fixture);

        let (migration, resolved) = run(&fixture);

        assert!(notified(&migration).is_empty());
        assert_resolved_everywhere(&resolved, CURRENT);
    }

    #[test]
    fn a_legacy_build_running_after_the_move_does_not_pull_this_one_back() {
        // An earlier build launched after the move recreates its directories;
        // the moved data in the current ones stays in use.
        let fixture = fixture();
        populate_legacy(&fixture);
        let current_config = fixture.roaming.join(CURRENT.app_directory_name);
        let current_data = fixture.local.join(CURRENT.app_directory_name);
        write(&current_config.join("settings.json"), "{\"current\": true}");
        write(
            &current_data.join("db/0-stable/db.sqlite"),
            "current database",
        );

        let (migration, resolved) = run_with(&fixture, false, &|_| true, &|from, to| {
            panic!("nothing may be moved, but {from:?} was moved to {to:?}")
        });

        for directory in outcomes(&migration) {
            assert!(
                matches!(directory.outcome, MigrationOutcome::CurrentAlreadyExists),
                "{directory:?}"
            );
        }
        assert!(migration.legacy_directories_in_use().is_empty());
        assert_resolved_everywhere(&resolved, CURRENT);
    }

    #[test]
    fn a_running_legacy_build_keeps_its_directories_in_use() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let checks = Cell::new(0);
        let seen_directories = std::cell::RefCell::new(Vec::new());

        let (migration, resolved) = run_with(
            &fixture,
            false,
            &|legacy| {
                checks.set(checks.get() + 1);
                assert_eq!(legacy.names, LEGACY);
                seen_directories.replace(legacy.directories.to_vec());
                true
            },
            &|from, to| panic!("nothing may be moved, but {from:?} was moved to {to:?}"),
        );

        assert_eq!(checks.get(), 1, "the running build is looked for once");
        assert_eq!(
            seen_directories.into_inner(),
            vec![
                fixture.roaming.join(LEGACY.app_directory_name),
                fixture.local.join(LEGACY.app_directory_name),
            ]
        );
        assert_eq!(migration.legacy_directories_in_use().len(), 2);
        assert!(migration.legacy_build_is_running());
        assert_resolved_everywhere(&resolved, LEGACY);
        assert!(!fixture.roaming.join(CURRENT.app_directory_name).exists());
        assert!(!fixture.local.join(CURRENT.app_directory_name).exists());
        assert_eq!(
            read(
                &fixture
                    .local
                    .join(LEGACY.app_directory_name)
                    .join("db/0-stable/db.sqlite")
            ),
            "legacy database"
        );
    }

    #[test]
    fn a_failed_move_keeps_that_directory_in_use_without_an_error() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let local_legacy = fixture.local.join(LEGACY.app_directory_name);

        let (migration, resolved) = run_with(&fixture, false, &|_| false, &|from, to| {
            if from == local_legacy {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "in use"))
            } else {
                fs::rename(from, to)
            }
        });

        let directories = outcomes(&migration);
        assert!(
            matches!(directories[0].outcome, MigrationOutcome::MovedBack),
            "the configuration directory follows the data directory back: {directories:?}"
        );
        assert!(matches!(
            directories[1].outcome,
            MigrationOutcome::MoveFailed(_)
        ));
        assert_eq!(migration.legacy_directories_in_use().len(), 2);
        assert!(!migration.legacy_build_is_running());
        assert_resolved_everywhere(&resolved, LEGACY);
        assert_eq!(
            read(&local_legacy.join("db/0-stable/db.sqlite")),
            "legacy database"
        );
        assert_eq!(
            read(
                &fixture
                    .roaming
                    .join(LEGACY.app_directory_name)
                    .join("settings.json")
            ),
            "{\"legacy\": true}"
        );
        assert!(!fixture.local.join(CURRENT.app_directory_name).exists());
        assert!(!fixture.roaming.join(CURRENT.app_directory_name).exists());
    }

    #[test]
    fn a_directory_that_cannot_be_moved_back_stays_moved_and_in_use() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let local_legacy = fixture.local.join(LEGACY.app_directory_name);
        let roaming_current = fixture.roaming.join(CURRENT.app_directory_name);

        let (migration, resolved) = run_with(&fixture, false, &|_| false, &|from, to| {
            if from == local_legacy {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "in use"))
            } else if from == roaming_current {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "in use"))
            } else {
                fs::rename(from, to)
            }
        });

        let directories = outcomes(&migration);
        assert!(
            matches!(
                &directories[0].outcome,
                MigrationOutcome::Moved {
                    move_back_error: Some(_),
                    ..
                }
            ),
            "{directories:?}"
        );
        assert_eq!(resolved.names(DirectoryKind::Config), CURRENT);
        assert_eq!(resolved.names(DirectoryKind::Data), LEGACY);
        assert_eq!(
            read(&roaming_current.join("settings.json")),
            "{\"legacy\": true}"
        );
    }

    #[test]
    fn an_empty_current_directory_does_not_strand_the_legacy_one() {
        let fixture = fixture();
        populate_legacy(&fixture);
        // What an installer, or a launch with `--user-data-dir` creating the
        // real temporary directory, leaves behind.
        let local_current = fixture.local.join(CURRENT.app_directory_name);
        fs::create_dir_all(local_current.join("logs")).expect("empty current directories");
        fs::create_dir_all(local_current.join("db").join("0-stable")).expect("empty nested");

        let (migration, resolved) = run(&fixture);

        for directory in outcomes(&migration) {
            assert!(
                matches!(&directory.outcome, MigrationOutcome::Moved { .. }),
                "{directory:?}"
            );
        }
        assert_resolved_everywhere(&resolved, CURRENT);
        assert_eq!(
            read(&local_current.join("db/0-stable/db.sqlite")),
            "legacy database"
        );
        assert!(!fixture.local.join(LEGACY.app_directory_name).exists());
    }

    #[test]
    fn a_current_directory_holding_a_file_is_not_replaced() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let local_current = fixture.local.join(CURRENT.app_directory_name);
        write(&local_current.join("logs").join("crash-handler"), "");

        let (migration, resolved) = run(&fixture);

        let directories = outcomes(&migration);
        let local = directories
            .iter()
            .find(|directory| directory.current == local_current)
            .expect("the local directory is checked");
        assert!(
            matches!(local.outcome, MigrationOutcome::CurrentAlreadyExists),
            "{directories:?}"
        );
        assert_eq!(resolved.names(DirectoryKind::Data), CURRENT);
        assert!(local_current.join("logs").join("crash-handler").exists());
        assert_eq!(
            read(
                &fixture
                    .local
                    .join(LEGACY.app_directory_name)
                    .join("db/0-stable/db.sqlite")
            ),
            "legacy database"
        );
    }

    #[test]
    fn resolving_without_moving_matches_what_the_migration_left_in_use() {
        let fixture = fixture();
        populate_legacy(&fixture);
        // The configuration directory is current and holds settings; the
        // data directory only exists empty under the current name.
        write(
            &fixture
                .roaming
                .join(CURRENT.app_directory_name)
                .join("settings.json"),
            "{}",
        );
        let local_current = fixture.local.join(CURRENT.app_directory_name);
        fs::create_dir_all(&local_current).expect("empty current directory");

        let resolved = resolve_without_moving(&fixture.locations, branding::LEGACY);

        assert_eq!(resolved.names(DirectoryKind::Config), CURRENT);
        for kind in [
            DirectoryKind::Data,
            DirectoryKind::State,
            DirectoryKind::Temp,
        ] {
            assert_eq!(resolved.names(kind), LEGACY, "{kind:?}");
        }
        assert!(local_current.exists(), "nothing is removed");
        assert!(fixture.local.join(LEGACY.app_directory_name).exists());
        assert!(fixture.roaming.join(LEGACY.app_directory_name).exists());

        let empty = tempfile::tempdir().expect("temporary directory");
        let locations = vec![DirectoryLocation {
            kinds: vec![DirectoryKind::Config],
            parent: empty.path().to_path_buf(),
            lowercase: false,
            logs_subdirectory: None,
        }];
        assert_eq!(
            resolve_without_moving(&locations, branding::LEGACY).names(DirectoryKind::Config),
            CURRENT
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_directory_with_an_open_file_is_left_in_use() {
        let fixture = fixture();
        populate_legacy(&fixture);
        let local_legacy = fixture.local.join(LEGACY.app_directory_name);
        let open_database =
            fs::File::open(local_legacy.join("db/0-stable/db.sqlite")).expect("open database");

        let (migration, resolved) = run(&fixture);

        let directories = outcomes(&migration);
        assert!(
            matches!(directories[1].outcome, MigrationOutcome::MoveFailed(_)),
            "{directories:?}"
        );
        assert_eq!(resolved.names(DirectoryKind::Data), LEGACY);
        assert!(local_legacy.join("db/0-stable/db.sqlite").exists());
        drop(open_database);
    }

    #[test]
    fn a_custom_data_directory_moves_nothing() {
        let fixture = fixture();
        populate_legacy(&fixture);

        let (migration, resolved) = run_with(
            &fixture,
            true,
            &|_| panic!("nothing is looked for"),
            &|from, to| panic!("nothing may be moved, but {from:?} was moved to {to:?}"),
        );

        assert!(matches!(migration, LegacyMigration::CustomDataDirectory));
        assert_resolved_everywhere(&resolved, CURRENT);
        assert!(fixture.roaming.join(LEGACY.app_directory_name).exists());
        assert!(!fixture.roaming.join(CURRENT.app_directory_name).exists());
    }

    #[test]
    fn official_zed_directories_are_never_touched() {
        let fixture = fixture();
        // The two spellings are one directory on a case-insensitive file
        // system, so each leaves a file of its own, named apart by more than
        // case.
        const OFFICIAL_NAMES: [(&str, &str); 2] = [("Zed", "capitalized"), ("zed", "lowercase")];
        let official_files = |name: &str, tag: &str| {
            [
                fixture
                    .roaming
                    .join(name)
                    .join(format!("settings-{tag}.json")),
                fixture
                    .local
                    .join(name)
                    .join(format!("db/0-stable/db-{tag}.sqlite")),
            ]
        };
        for (name, tag) in OFFICIAL_NAMES {
            for file in official_files(name, tag) {
                write(&file, name);
            }
        }
        let (migration, resolved) = run(&fixture);
        assert!(outcomes(&migration).is_empty());
        assert_resolved_everywhere(&resolved, CURRENT);

        populate_legacy(&fixture);
        let moved = std::cell::RefCell::new(Vec::new());
        let (migration, _) = run_with(&fixture, false, &|_| false, &|from, to| {
            moved.borrow_mut().push(from.to_path_buf());
            fs::rename(from, to)
        });

        assert_eq!(outcomes(&migration).len(), 2);
        for from in moved.into_inner() {
            let name = from
                .strip_prefix(&fixture.roaming)
                .or_else(|_| from.strip_prefix(&fixture.local))
                .ok()
                .and_then(|relative| relative.components().next())
                .map(|component| component.as_os_str().to_string_lossy().into_owned());
            let name = name.unwrap_or_else(|| panic!("{from:?} is outside the fixture"));
            assert!(
                !name.eq_ignore_ascii_case("zed"),
                "{from:?} is inside an official directory"
            );
        }
        for (name, tag) in OFFICIAL_NAMES {
            for file in official_files(name, tag) {
                assert_eq!(read(&file), name);
            }
        }
        assert!(
            !fixture
                .roaming
                .join(CURRENT.app_directory_name)
                .join("settings-capitalized.json")
                .exists()
        );
    }

    #[test]
    fn lowercase_locations_use_the_lowercase_names() {
        let root = tempfile::tempdir().expect("temporary directory");
        let config_home = root.path().join("config");
        write(
            &config_home
                .join(LEGACY.app_directory_name_lowercase)
                .join("settings.json"),
            "legacy",
        );
        let locations = vec![DirectoryLocation {
            kinds: vec![DirectoryKind::Config],
            parent: config_home.clone(),
            lowercase: true,
            logs_subdirectory: None,
        }];

        let (migration, _) = Migration {
            locations: &locations,
            legacy_sets: branding::LEGACY,
            custom_data_dir_is_set: false,
            legacy_build_is_running: &|_| false,
            rename: &|from, to| fs::rename(from, to),
        }
        .run();

        assert!(matches!(
            outcomes(&migration)[0].outcome,
            MigrationOutcome::Moved { .. }
        ));
        assert_eq!(
            read(
                &config_home
                    .join(CURRENT.app_directory_name_lowercase)
                    .join("settings.json")
            ),
            "legacy"
        );
    }

    #[test]
    fn a_live_remote_server_pid_file_counts_as_running() {
        let root = tempfile::tempdir().expect("temporary directory");
        let legacy_data = root.path().join(LEGACY.app_directory_name_lowercase);
        write(
            &legacy_data.join("server_state/d-0123/daemon.pid"),
            "4242\n",
        );
        write(
            &legacy_data.join("server_state/workspace-1/server.pid"),
            "77",
        );
        let directories = vec![legacy_data];

        assert!(remote_server_is_running(&directories, &|pid| pid == 4242));
        assert!(remote_server_is_running(&directories, &|pid| pid == 77));
        assert!(!remote_server_is_running(&directories, &|_| false));
        assert!(!remote_server_is_running(&[], &|_| true));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_instance_mutex_counts_as_running() {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::HSTRING;

        let identifier = format!(
            "ForgeKairos-legacy-migration-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        assert!(!platform::windows_instance_mutex_exists([
            identifier.as_str()
        ]));

        // SAFETY: the name is a valid wide string that outlives the call.
        let mutex = unsafe {
            CreateMutexW(
                None,
                false,
                &HSTRING::from(format!("{identifier}-Instance-Mutex")),
            )
        }
        .expect("create mutex");
        assert!(platform::windows_instance_mutex_exists([
            "some-other-identifier",
            identifier.as_str()
        ]));
        // SAFETY: `mutex` was created above and is closed once.
        unsafe { CloseHandle(mutex) }.expect("close mutex");
        assert!(!platform::windows_instance_mutex_exists([
            identifier.as_str()
        ]));
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn a_listening_instance_socket_counts_as_running() {
        use std::os::unix::net::UnixDatagram;

        let root = tempfile::tempdir().expect("temporary directory");
        let directories = vec![root.path().to_path_buf()];
        assert!(!platform::linux_instance_socket_is_listening(&directories));
        let listener =
            UnixDatagram::bind(root.path().join("zed-stable.sock")).expect("bind socket");
        assert!(platform::linux_instance_socket_is_listening(&directories));
        drop(listener);
        assert!(!platform::linux_instance_socket_is_listening(&directories));
    }

    #[test]
    fn the_macos_instance_check_probes_the_legacy_and_the_current_instances() {
        let user_id = 501;
        let probes = platform::macos_instance_probes(user_id);
        let legacy = [
            (43737 + 501, "Zed Editor Dev Instance Running"),
            (43837 + 501, "Zed Editor Preview Instance Running"),
            (43937 + 501, "Zed Editor Stable Instance Running"),
            (44037 + 501, "Zed Editor Nightly Instance Running"),
        ];
        for (port, handshake) in legacy {
            assert!(
                probes.contains(&(port, handshake.to_string())),
                "{probes:?} misses the legacy instance on {port}"
            );
        }
        let identifiers = branding::CURRENT.app_identifiers;
        let current = [
            (33737 + 501, identifiers.dev),
            (33837 + 501, identifiers.preview),
            (33937 + 501, identifiers.stable),
            (34037 + 501, identifiers.nightly),
        ];
        for (port, identifier) in current {
            let handshake = format!("{identifier} Instance Running");
            assert!(
                probes.contains(&(port, handshake.clone())),
                "{probes:?} misses this build's instance {handshake:?} on {port}"
            );
        }
        assert_eq!(probes.len(), legacy.len() + current.len());
    }

    #[test]
    fn the_platform_locations_cover_every_kind_this_platform_resolves() {
        let locations = platform::locations();
        let mut kinds = vec![
            DirectoryKind::Config,
            DirectoryKind::State,
            DirectoryKind::Temp,
        ];
        if cfg!(any(
            target_os = "windows",
            target_os = "macos",
            target_os = "linux",
            target_os = "freebsd"
        )) {
            kinds.push(DirectoryKind::Data);
        }
        if cfg!(target_os = "macos") {
            kinds.push(DirectoryKind::Logs);
        }
        for kind in kinds {
            assert_eq!(
                locations
                    .iter()
                    .filter(|location| location.kinds.contains(&kind))
                    .count(),
                1,
                "{kind:?} in {locations:?}"
            );
        }
        assert_eq!(
            locations
                .iter()
                .filter(|location| location.logs_subdirectory.is_some())
                .count(),
            1,
            "{locations:?}"
        );
        if cfg!(target_os = "windows") {
            assert!(
                locations
                    .first()
                    .is_some_and(|location| location.kinds.contains(&DirectoryKind::Data)),
                "the directory a running build keeps files open in goes first: {locations:?}"
            );
        }
    }
}
