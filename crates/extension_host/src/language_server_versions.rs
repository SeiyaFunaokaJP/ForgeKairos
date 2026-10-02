use collections::HashSet;
use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use std::{
    path::{Component, Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime},
};
use util::ResultExt as _;

/// How long an older version of a language server has to have sat untouched
/// before it is removed. Another release channel sharing this extension
/// directory may still be pinned to it; a month's grace bounds how often it
/// has to download it again.
pub(crate) const SUPERSEDED_VERSION_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Removes the versions of a language server an extension downloaded before
/// the one it has just started, which the extension itself leaves behind
/// unless it clears them out on its own.
///
/// An extension's work directory is laid out however the extension likes, so
/// only entries named like the one in use — the same name, followed by a
/// different version (`gleam-v1.2.3` next to `gleam-v1.2.2`) — are taken
/// for older copies. Anything else is the extension's business and stays.
pub(crate) struct SupersededVersionSweep<'a> {
    pub extension_work_dir: &'a Path,
    /// The language server command and its arguments, as the extension
    /// returned them.
    pub command_paths: &'a [PathBuf],
    pub now: SystemTime,
    /// Whether a running process was started from, or was handed, a path
    /// inside this one: an extension that runs several versions side by
    /// side, one per project, is using all of them.
    pub is_used_by_a_process: &'a (dyn Fn(&Path) -> bool + Send + Sync),
}

impl SupersededVersionSweep<'_> {
    pub async fn run(&self, fs: &dyn Fs) -> Vec<PathBuf> {
        let mut removed = Vec::new();
        let Some(mut listing) = fs.read_dir(self.extension_work_dir).await.log_err() else {
            return removed;
        };
        let mut entries = Vec::new();
        while let Some(entry) = listing.next().await {
            if let Some(path) = entry.log_err() {
                entries.push(path);
            }
        }
        let entry_names = entries
            .iter()
            .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
            .collect::<HashSet<_>>();

        // An argument that names nothing in the work directory is not a path
        // at all (`--runtime jvm-17`), and must not make `jvm-11` an older copy
        // of something in use.
        let in_use_entries = in_use_entries(self.extension_work_dir, self.command_paths)
            .into_iter()
            .filter(|entry| entry_names.contains(entry.as_str()))
            .collect::<Vec<_>>();
        let in_use_names = in_use_entries
            .iter()
            .filter_map(|entry| split_version(entry).map(|(name, _)| name))
            .collect::<HashSet<_>>();
        if in_use_names.is_empty() {
            return removed;
        }

        for path in entries {
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if in_use_entries.iter().any(|entry| entry == file_name) {
                continue;
            }
            let Some((name, _)) = split_version(file_name) else {
                continue;
            };
            if !in_use_names.contains(name) {
                continue;
            }
            let Some(metadata) = fs.metadata(&path).await.log_err().flatten() else {
                continue;
            };
            if metadata.is_symlink {
                continue;
            }
            if !self.is_untouched_for_long(metadata.mtime.timestamp_for_user()) {
                continue;
            }
            if (self.is_used_by_a_process)(&path) {
                continue;
            }
            // Another project may have just picked this version to start,
            // which marks it as used before the process exists: looking again
            // after the process list leaves only the moment of the removal
            // itself for that to slip through.
            let still_untouched =
                fs.metadata(&path)
                    .await
                    .log_err()
                    .flatten()
                    .is_some_and(|metadata| {
                        self.is_untouched_for_long(metadata.mtime.timestamp_for_user())
                    });
            if !still_untouched {
                continue;
            }
            let options = RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            };
            let result = if metadata.is_dir {
                fs.remove_dir(&path, options).await
            } else {
                fs.remove_file(&path, options).await
            };
            match result {
                Ok(()) => {
                    log::info!("removed superseded language server version {path:?}");
                    removed.push(path);
                }
                Err(error) => {
                    log::warn!("could not remove superseded language server {path:?}: {error:#}");
                }
            }
        }
        removed
    }

    fn is_untouched_for_long(&self, modified: SystemTime) -> bool {
        self.now
            .duration_since(modified)
            .is_ok_and(|age| age > SUPERSEDED_VERSION_AGE)
    }
}

/// The entries of the work directory the command runs from.
fn in_use_entries(extension_work_dir: &Path, command_paths: &[PathBuf]) -> Vec<String> {
    command_paths
        .iter()
        .filter_map(|path| {
            let path = extension_work_dir.join(from_wasi_path(path));
            let path = util::paths::normalize_lexically(&path).ok()?;
            let relative = path.strip_prefix(extension_work_dir).ok()?;
            match relative.components().next()? {
                Component::Normal(entry) => entry.to_str().map(str::to_string),
                _ => None,
            }
        })
        .collect()
}

/// Records that the entries of the work directory the command runs from are
/// in use, by moving their modification time to `now`. Without this a
/// version's age would be the time it was downloaded, and one that a channel
/// still starts every day would count as unused a month after it arrived.
///
/// Done before the server starts, so that a sweep running elsewhere at the
/// same moment sees the version as fresh rather than as unused.
pub(crate) fn mark_in_use(extension_work_dir: &Path, command_paths: &[PathBuf], now: SystemTime) {
    let mut entries = in_use_entries(extension_work_dir, command_paths);
    entries.sort();
    entries.dedup();
    for entry in entries {
        let path = extension_work_dir.join(entry);
        match set_modified(&path, now) {
            Ok(()) => {}
            // Not a path after all, only an argument that looked like one.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => log::warn!("could not mark {path:?} as in use: {error}"),
        }
    }
}

fn set_modified(path: &Path, modified: SystemTime) -> std::io::Result<()> {
    // A link's own time is not what the sweep reads, and following it would
    // touch whatever it points at.
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Ok(());
    }
    let mut options = std::fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // Setting a time needs only the right to write attributes, and a
        // directory opens only with backup semantics.
        options
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    #[cfg(not(windows))]
    options.read(true);
    options.open(path)?.set_modified(modified)
}

/// Undoes the leading slash WASI puts in front of a Windows path
/// (`/C:/Users/...`), which would otherwise read as a relative path.
fn from_wasi_path(path: &Path) -> &Path {
    if cfg!(windows)
        && let Some(text) = path.to_str()
        && let Some(stripped) = text.strip_prefix('/')
    {
        let mut characters = stripped.chars();
        if characters.next().is_some_and(|c| c.is_ascii_alphabetic())
            && characters.next() == Some(':')
            && matches!(characters.next(), Some('/' | '\\'))
        {
            return Path::new(stripped);
        }
    }
    path
}

/// Splits `gleam-v1.2.3` into `gleam` and `v1.2.3`: the name is everything
/// before the first `-` or `_` that a version number follows.
fn split_version(file_name: &str) -> Option<(&str, &str)> {
    let bytes = file_name.as_bytes();
    (1..bytes.len()).find_map(|index| {
        if !matches!(bytes[index], b'-' | b'_') {
            return None;
        }
        let version = &file_name[index + 1..];
        let number = version
            .strip_prefix(['v', 'V'])
            .unwrap_or(version)
            .as_bytes();
        number
            .first()
            .is_some_and(u8::is_ascii_digit)
            .then(|| (&file_name[..index], version))
    })
}

/// Whether any process on this machine runs from, or was given an argument
/// inside, `path`. Asks the system at most once per sweep.
pub(crate) fn process_probe() -> impl Fn(&Path) -> bool + Send + Sync {
    let processes = OnceLock::<Vec<(Option<PathBuf>, Vec<PathBuf>)>>::new();
    move |path| {
        let processes = processes.get_or_init(|| {
            let system = sysinfo::System::new_with_specifics(
                sysinfo::RefreshKind::nothing().with_processes(
                    sysinfo::ProcessRefreshKind::nothing()
                        .with_exe(sysinfo::UpdateKind::Always)
                        .with_cmd(sysinfo::UpdateKind::Always),
                ),
            );
            system
                .processes()
                .values()
                .map(|process| {
                    (
                        process.exe().map(Path::to_path_buf),
                        process.cmd().iter().map(PathBuf::from).collect(),
                    )
                })
                .collect()
        });
        processes.iter().any(|(executable, arguments)| {
            executable
                .as_deref()
                .is_some_and(|executable| executable.starts_with(path))
                || arguments.iter().any(|argument| argument.starts_with(path))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use std::sync::Arc;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    const WORK_DIR: &str = "/extensions/work/gleam";

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + 1000 * DAY
    }

    fn long_ago() -> SystemTime {
        now() - SUPERSEDED_VERSION_AGE - DAY
    }

    fn work_path(relative: &str) -> PathBuf {
        Path::new(WORK_DIR).join(relative)
    }

    async fn file_at(fs: &FakeFs, relative: &str, modified: SystemTime) {
        let path = work_path(relative);
        if let Some(parent) = path.parent() {
            fs.set_next_mtime(modified);
            fs.create_dir(parent)
                .await
                .expect("create the parent directory");
        }
        fs.set_next_mtime(modified);
        fs.insert_file(&path, b"binary".to_vec()).await;
    }

    async fn sweep(
        fs: &Arc<FakeFs>,
        command_paths: &[&str],
        paths_in_use: &[&str],
    ) -> Vec<PathBuf> {
        let command_paths = command_paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        let paths_in_use = paths_in_use
            .iter()
            .map(|path| work_path(path))
            .collect::<Vec<_>>();
        let mut removed = SupersededVersionSweep {
            extension_work_dir: Path::new(WORK_DIR),
            command_paths: &command_paths,
            now: now(),
            is_used_by_a_process: &move |path| {
                paths_in_use.iter().any(|in_use| in_use.starts_with(path))
            },
        }
        .run(fs.as_ref())
        .await;
        removed.sort();
        removed
    }

    #[gpui::test]
    async fn older_versions_of_the_server_in_use_are_removed(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.3/gleam", now() - DAY).await;
        file_at(&fs, "gleam-v1.2.2/gleam", long_ago()).await;
        file_at(&fs, "gleam-v1.1.0/gleam", long_ago()).await;
        file_at(&fs, "gleam-1.0.0", long_ago()).await;

        let removed = sweep(&fs, &["gleam-v1.2.3/gleam"], &[]).await;

        assert_eq!(
            removed,
            [
                work_path("gleam-1.0.0"),
                work_path("gleam-v1.1.0"),
                work_path("gleam-v1.2.2"),
            ]
        );
        assert!(fs.is_file(&work_path("gleam-v1.2.3/gleam")).await);
    }

    #[gpui::test]
    async fn anything_not_named_like_the_server_in_use_stays(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.3/gleam", now() - DAY).await;
        // Another language server of the same extension.
        file_at(&fs, "gleam-lsp-proxy-0.3.0/proxy", long_ago()).await;
        // Things the extension keeps without a version in the name.
        file_at(&fs, "node_modules/typescript/package.json", long_ago()).await;
        file_at(&fs, "package.json", long_ago()).await;
        file_at(&fs, "gleam/config", long_ago()).await;

        let removed = sweep(&fs, &["gleam-v1.2.3/gleam"], &[]).await;

        assert_eq!(removed, Vec::<PathBuf>::new());
    }

    #[gpui::test]
    async fn recent_or_running_versions_stay(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.3/gleam", now() - DAY).await;
        // Downloaded a week ago, by another release channel perhaps.
        file_at(&fs, "gleam-v1.3.0/gleam", now() - 7 * DAY).await;
        // Old, but serving another project right now.
        file_at(&fs, "gleam-v1.0.0/gleam", long_ago()).await;
        file_at(&fs, "gleam-v0.9.0/gleam", long_ago()).await;

        let removed = sweep(&fs, &["gleam-v1.2.3/gleam"], &["gleam-v1.0.0/gleam"]).await;

        assert_eq!(removed, [work_path("gleam-v0.9.0")]);
        assert!(fs.is_file(&work_path("gleam-v1.3.0/gleam")).await);
        assert!(fs.is_file(&work_path("gleam-v1.0.0/gleam")).await);
    }

    #[gpui::test]
    async fn every_version_the_command_names_stays(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "server-2.0.0/server.js", long_ago()).await;
        file_at(&fs, "server-1.0.0/server.js", long_ago()).await;
        file_at(&fs, "server-0.5.0/server.js", long_ago()).await;

        // Run by a node binary outside the work directory, which hands it the
        // script inside.
        let removed = sweep(
            &fs,
            &[
                "/usr/bin/node",
                &format!("{WORK_DIR}/server-2.0.0/server.js"),
                "--stdio",
                "--plugin=server-1.0.0/plugin.js",
                "server-1.0.0/plugin.js",
            ],
            &[],
        )
        .await;

        assert_eq!(removed, [work_path("server-0.5.0")]);
    }

    #[gpui::test]
    async fn a_command_outside_the_work_directory_removes_nothing(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.2/gleam", long_ago()).await;

        let removed = sweep(&fs, &["/usr/local/bin/gleam-v1.2.3", "lsp"], &[]).await;

        assert_eq!(removed, Vec::<PathBuf>::new());
    }

    #[gpui::test]
    async fn an_argument_naming_nothing_in_the_work_directory_is_no_version_in_use(
        cx: &mut TestAppContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.3/gleam", now() - DAY).await;
        // Something of the extension's own that only shares a name with the
        // argument below.
        file_at(&fs, "jvm-11/release", long_ago()).await;

        let removed = sweep(&fs, &["gleam-v1.2.3/gleam", "--runtime", "jvm-17"], &[]).await;
        assert_eq!(removed, Vec::<PathBuf>::new());
        assert!(fs.is_file(&work_path("jvm-11/release")).await);

        // Once the argument does name an entry, it is a version in use.
        file_at(&fs, "jvm-17/release", now() - DAY).await;
        let removed = sweep(&fs, &["gleam-v1.2.3/gleam", "--runtime", "jvm-17"], &[]).await;
        assert_eq!(removed, [work_path("jvm-11")]);
    }

    #[gpui::test]
    async fn a_version_another_project_starts_during_the_sweep_stays(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        file_at(&fs, "gleam-v1.2.3/gleam", now() - DAY).await;
        file_at(&fs, "gleam-v1.0.0/gleam", long_ago()).await;
        file_at(&fs, "gleam-v0.9.0/gleam", long_ago()).await;

        let command_paths = [PathBuf::from("gleam-v1.2.3/gleam")];
        let started_meanwhile = work_path("gleam-v1.0.0");
        let removed = SupersededVersionSweep {
            extension_work_dir: Path::new(WORK_DIR),
            command_paths: &command_paths,
            now: now(),
            // The other project marks the version as used after the sweep
            // read its time, and its process is not running yet when the
            // sweep looks for one.
            is_used_by_a_process: &{
                let fs = fs.clone();
                move |path| {
                    if path == started_meanwhile {
                        fs.set_next_mtime(now());
                        futures::executor::block_on(fs.touch_path(path));
                    }
                    false
                }
            },
        }
        .run(fs.as_ref())
        .await;

        assert_eq!(removed, [work_path("gleam-v0.9.0")]);
        assert!(fs.is_file(&work_path("gleam-v1.0.0/gleam")).await);
    }

    #[test]
    fn starting_a_server_marks_the_entries_it_runs_from_as_used() {
        let temp_dir = tempfile::tempdir().expect("create a temporary directory");
        let work_dir = temp_dir.path();
        let in_use = work_dir.join("gleam-v1.2.3");
        let sibling = work_dir.join("gleam-v1.2.2");
        for directory in [&in_use, &sibling] {
            std::fs::create_dir(directory).expect("create a version directory");
            std::fs::write(directory.join("gleam"), b"binary").expect("write the binary");
        }
        let downloaded_at = SystemTime::now() - SUPERSEDED_VERSION_AGE - DAY;
        for directory in [&in_use, &sibling] {
            set_modified(directory, downloaded_at).expect("age the version directory");
        }
        let modified = |path: &Path| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .expect("read the modification time")
        };
        assert!(modified(&in_use) < SystemTime::now() - SUPERSEDED_VERSION_AGE);

        let now = SystemTime::now();
        mark_in_use(
            work_dir,
            &[
                in_use.join("gleam"),
                PathBuf::from("--stdio"),
                PathBuf::from("missing-1.0.0/server"),
            ],
            now,
        );

        let marked_at = modified(&in_use);
        let difference = marked_at
            .duration_since(now)
            .unwrap_or_else(|error| error.duration());
        assert!(
            difference < Duration::from_secs(2),
            "the version in use is marked as used now, not {marked_at:?}"
        );
        assert!(
            modified(&sibling) < now - SUPERSEDED_VERSION_AGE,
            "an older version is left as it was"
        );
        assert!(!work_dir.join("missing-1.0.0").exists());
    }

    #[test]
    fn split_version_finds_the_first_version_number() {
        assert_eq!(split_version("gleam-v1.2.3"), Some(("gleam", "v1.2.3")));
        assert_eq!(split_version("zls-0.13.0"), Some(("zls", "0.13.0")));
        assert_eq!(split_version("clangd_18.1.3"), Some(("clangd", "18.1.3")));
        assert_eq!(
            split_version("elixir-ls-v0.20.0"),
            Some(("elixir-ls", "v0.20.0"))
        );
        assert_eq!(
            split_version("marksman-2024-12-18"),
            Some(("marksman", "2024-12-18"))
        );
        assert_eq!(split_version("node_modules"), None);
        assert_eq!(split_version("gleam"), None);
        assert_eq!(split_version("-1.0"), None);
        assert_eq!(split_version("gleam-"), None);
    }

    #[test]
    fn process_probe_sees_this_test_binary() {
        let probe = process_probe();
        let this_executable = std::env::current_exe().expect("find this test binary");
        let directory = this_executable
            .parent()
            .expect("the test binary has a directory");
        assert!(probe(directory));
        assert!(!probe(Path::new("/no/such/directory/anywhere")));
    }
}
