use anyhow::{Context as _, Result};
use collections::HashMap;
use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
use util::ResultExt as _;

/// A state directory whose daemon has not started or stopped for this long
/// serves no binary a client still connects with: every new build and every
/// client machine gets a directory of its own (ADR-F45), and the ones left
/// behind would otherwise pile up forever.
pub(crate) const STALE_STATE_DIR_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How often a running daemon marks its state directory as in use, well
/// within [`STALE_STATE_DIR_AGE`].
pub(crate) const STATE_DIR_HEARTBEAT: Duration = Duration::from_secs(24 * 60 * 60);
const _: () = assert!(STATE_DIR_HEARTBEAT.as_secs() * 7 < STALE_STATE_DIR_AGE.as_secs());
/// How many daemons' logs are kept once their daemons are gone, newest first.
pub(crate) const KEPT_LOG_GENERATIONS: usize = 5;
/// Everything the logs of gone daemons may take up together.
pub(crate) const KEPT_LOG_BYTES: u64 = 20 * 1024 * 1024;
/// A log written to this recently may belong to a daemon still starting up,
/// which writes its log before its pid file.
pub(crate) const RECENT_LOG_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const PID_FILE_NAMES: [&str; 2] = ["daemon.pid", "server.pid"];
const LOCK_FILE_NAME: &str = "daemon.lock";
const LOG_PREFIX: &str = "server-";
const LOG_SUFFIX: &str = ".log";
const ROTATED_LOG_SUFFIX: &str = ".1.log";

pub(crate) struct StaleFileSweep<'a> {
    pub state_root: &'a Path,
    pub logs_dir: &'a Path,
    /// The state directory of the daemon running the sweep.
    pub own_state_dir: &'a Path,
    /// The log of the daemon running the sweep.
    pub own_log_file: &'a Path,
    pub now: SystemTime,
    pub pid_is_alive: &'a (dyn Fn(u32) -> bool + Send + Sync),
    /// Whether a proxy is holding this lock file right now, which it does
    /// while it starts a daemon in the directory.
    pub lock_is_held: &'a (dyn Fn(&Path) -> bool + Send + Sync),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepOutcome {
    pub removed_state_dirs: Vec<PathBuf>,
    pub removed_logs: Vec<PathBuf>,
}

impl StaleFileSweep<'_> {
    pub async fn run(&self, fs: &dyn Fs) -> SweepOutcome {
        let mut outcome = SweepOutcome::default();
        let live_state_names = self
            .sweep_state_dirs(fs, &mut outcome)
            .await
            .context("sweeping stale remote server state directories")
            .log_err()
            .unwrap_or_default();
        self.sweep_logs(fs, &live_state_names, &mut outcome)
            .await
            .context("sweeping stale remote server logs")
            .log_err();
        outcome
    }

    /// Removes the state directories nobody has used for a long time, and
    /// returns the names of the ones something is still running in.
    async fn sweep_state_dirs(
        &self,
        fs: &dyn Fs,
        outcome: &mut SweepOutcome,
    ) -> Result<Vec<String>> {
        let mut live_names = Vec::new();
        let mut entries = fs.read_dir(self.state_root).await?;
        while let Some(entry) = entries.next().await {
            let Some(state_dir) = entry.log_err() else {
                continue;
            };
            let Some(name) = file_name_of(&state_dir) else {
                continue;
            };
            if state_dir == self.own_state_dir {
                live_names.push(name);
                continue;
            }
            let Some(metadata) = fs.metadata(&state_dir).await.log_err().flatten() else {
                continue;
            };
            if !metadata.is_dir || metadata.is_symlink {
                continue;
            }
            if self.is_state_dir_in_use(fs, &state_dir).await {
                live_names.push(name);
                continue;
            }
            let last_used = self.newest_mtime_in(fs, &state_dir).await;
            if !self.is_older_than(last_used, STALE_STATE_DIR_AGE) {
                continue;
            }
            match fs
                .remove_dir(
                    &state_dir,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await
            {
                Ok(()) => {
                    log::info!("removed stale remote server state directory {state_dir:?}");
                    outcome.removed_state_dirs.push(state_dir);
                }
                Err(error) => {
                    log::warn!("could not remove stale state directory {state_dir:?}: {error:#}");
                }
            }
        }
        Ok(live_names)
    }

    async fn is_state_dir_in_use(&self, fs: &dyn Fs, state_dir: &Path) -> bool {
        for pid_file_name in PID_FILE_NAMES {
            let pid = fs
                .load(&state_dir.join(pid_file_name))
                .await
                .ok()
                .and_then(|contents| contents.trim().parse::<u32>().ok());
            if pid.is_some_and(|pid| (self.pid_is_alive)(pid)) {
                return true;
            }
        }
        let lock_file = state_dir.join(LOCK_FILE_NAME);
        fs.is_file(&lock_file).await && (self.lock_is_held)(&lock_file)
    }

    /// The last time a daemon started or stopped in `state_dir`: starting one
    /// writes its pid file and socket, and stopping it removes them.
    async fn newest_mtime_in(&self, fs: &dyn Fs, state_dir: &Path) -> Option<SystemTime> {
        let mut newest = fs
            .metadata(state_dir)
            .await
            .log_err()
            .flatten()
            .map(|metadata| metadata.mtime.timestamp_for_user());
        let Some(mut entries) = fs.read_dir(state_dir).await.log_err() else {
            return newest;
        };
        while let Some(entry) = entries.next().await {
            let Some(entry) = entry.log_err() else {
                continue;
            };
            if let Some(metadata) = fs.metadata(&entry).await.log_err().flatten() {
                let modified = metadata.mtime.timestamp_for_user();
                newest = Some(newest.map_or(modified, |newest| newest.max(modified)));
            }
        }
        newest
    }

    fn is_older_than(&self, time: Option<SystemTime>, age: Duration) -> bool {
        // A time in the future, or none at all, says nothing about how long
        // the thing has been unused, so it is kept.
        time.and_then(|time| self.now.duration_since(time).ok())
            .is_some_and(|elapsed| elapsed > age)
    }

    async fn sweep_logs(
        &self,
        fs: &dyn Fs,
        live_state_names: &[String],
        outcome: &mut SweepOutcome,
    ) -> Result<()> {
        let mut logs_by_owner = HashMap::<String, ServerLog>::default();
        let mut entries = fs.read_dir(self.logs_dir).await?;
        while let Some(entry) = entries.next().await {
            let Some(path) = entry.log_err() else {
                continue;
            };
            let Some(owner) = file_name_of(&path).and_then(|name| log_owner(&name)) else {
                continue;
            };
            let Some(metadata) = fs.metadata(&path).await.log_err().flatten() else {
                continue;
            };
            if metadata.is_dir || metadata.is_symlink {
                continue;
            }
            let modified = metadata.mtime.timestamp_for_user();
            let log = logs_by_owner.entry(owner).or_default();
            log.paths.push(path);
            log.bytes += metadata.len;
            log.modified = Some(log.modified.map_or(modified, |newest| newest.max(modified)));
        }

        let own_log_owner = file_name_of(self.own_log_file).and_then(|name| log_owner(&name));
        let mut logs = logs_by_owner
            .into_iter()
            .map(|(owner, mut log)| {
                log.is_protected = own_log_owner.as_deref() == Some(owner.as_str())
                    || live_state_names.contains(&owner)
                    || !self.is_older_than(log.modified, RECENT_LOG_AGE);
                log
            })
            .collect::<Vec<_>>();
        logs.sort_by_key(|log| std::cmp::Reverse(log.modified));

        let mut kept_bytes = 0;
        let mut removable = Vec::new();
        for (index, log) in logs.iter().enumerate() {
            if log.is_protected {
                kept_bytes += log.bytes;
            } else if index < KEPT_LOG_GENERATIONS {
                kept_bytes += log.bytes;
                removable.push((log, false));
            } else {
                removable.push((log, true));
            }
        }
        // Oldest first, so that going over the size limit costs the logs
        // least likely to still be read.
        for (log, beyond_generations) in removable.into_iter().rev() {
            let remove = if beyond_generations {
                true
            } else if kept_bytes > KEPT_LOG_BYTES {
                kept_bytes -= log.bytes;
                true
            } else {
                false
            };
            if !remove {
                continue;
            }
            for path in &log.paths {
                match fs
                    .remove_file(
                        path,
                        RemoveOptions {
                            recursive: false,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await
                {
                    Ok(()) => {
                        log::info!("removed stale remote server log {path:?}");
                        outcome.removed_logs.push(path.clone());
                    }
                    Err(error) => {
                        log::warn!("could not remove stale log {path:?}: {error:#}");
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct ServerLog {
    paths: Vec<PathBuf>,
    bytes: u64,
    modified: Option<SystemTime>,
    is_protected: bool,
}

fn file_name_of(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

/// The name of the state directory whose daemon writes the log named
/// `file_name`, for both the log and its rotated older half.
fn log_owner(file_name: &str) -> Option<String> {
    let rest = file_name.strip_prefix(LOG_PREFIX)?;
    let owner = rest
        .strip_suffix(ROTATED_LOG_SUFFIX)
        .or_else(|| rest.strip_suffix(LOG_SUFFIX))?;
    (!owner.is_empty()).then(|| owner.to_string())
}

/// Whether some process has taken the lock at `path` — a proxy starting a
/// daemon in its directory.
pub(crate) fn lock_file_is_held(path: &Path) -> bool {
    let file = match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        // Unreadable means unknown, and an unknown directory is kept.
        Err(_) => return true,
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use std::sync::Arc;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    const STATE_ROOT: &str = "/home/user/.local/share/zed/server_state";
    const LOGS_DIR: &str = "/home/user/.local/share/zed/logs";

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + 1000 * DAY
    }

    async fn state_dir_used_at(
        fs: &FakeFs,
        name: &str,
        used_at: SystemTime,
        files: &[(&str, &str)],
    ) {
        let state_dir = Path::new(STATE_ROOT).join(name);
        fs.set_next_mtime(used_at);
        fs.create_dir(&state_dir)
            .await
            .expect("create the state directory");
        for (file_name, contents) in files {
            fs.set_next_mtime(used_at);
            fs.insert_file(state_dir.join(file_name), contents.as_bytes().to_vec())
                .await;
        }
    }

    async fn log_written_at(fs: &FakeFs, file_name: &str, written_at: SystemTime, bytes: usize) {
        fs.set_next_mtime(written_at);
        fs.insert_file(Path::new(LOGS_DIR).join(file_name), vec![b'x'; bytes])
            .await;
    }

    async fn sweep(fs: &Arc<FakeFs>, alive_pids: &[u32], held_locks: &[&str]) -> SweepOutcome {
        let state_root = PathBuf::from(STATE_ROOT);
        let own_state_dir = state_root.join("d-00000000");
        let logs_dir = PathBuf::from(LOGS_DIR);
        let own_log_file = logs_dir.join("server-d-00000000.log");
        let alive_pids = alive_pids.to_vec();
        let held_locks = held_locks
            .iter()
            .map(|name| state_root.join(name).join(LOCK_FILE_NAME))
            .collect::<Vec<_>>();
        StaleFileSweep {
            state_root: &state_root,
            logs_dir: &logs_dir,
            own_state_dir: &own_state_dir,
            own_log_file: &own_log_file,
            now: now(),
            pid_is_alive: &move |pid| alive_pids.contains(&pid),
            lock_is_held: &move |path| held_locks.iter().any(|held| held == path),
        }
        .run(fs.as_ref())
        .await
    }

    fn state_dir(name: &str) -> PathBuf {
        Path::new(STATE_ROOT).join(name)
    }

    fn log(name: &str) -> PathBuf {
        Path::new(LOGS_DIR).join(name)
    }

    async fn setup(cx: &mut TestAppContext) -> Arc<FakeFs> {
        let fs = FakeFs::new(cx.executor());
        fs.create_dir(Path::new(STATE_ROOT))
            .await
            .expect("create the state root");
        fs.create_dir(Path::new(LOGS_DIR))
            .await
            .expect("create the logs directory");
        fs
    }

    #[gpui::test]
    async fn state_dirs_unused_for_a_month_are_removed(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        let long_ago = now() - STALE_STATE_DIR_AGE - DAY;
        state_dir_used_at(&fs, "d-00000000", long_ago, &[("daemon.pid", "1")]).await;
        state_dir_used_at(&fs, "d-11111111", long_ago, &[("daemon.lock", "")]).await;
        state_dir_used_at(&fs, "workspace-12", long_ago, &[("server.pid", "12")]).await;
        state_dir_used_at(&fs, "d-22222222", now() - 3 * DAY, &[("daemon.lock", "")]).await;

        let outcome = sweep(&fs, &[], &[]).await;

        let mut removed = outcome.removed_state_dirs;
        removed.sort();
        assert_eq!(
            removed,
            [state_dir("d-11111111"), state_dir("workspace-12")]
        );
        assert!(
            fs.is_dir(&state_dir("d-00000000")).await,
            "the sweeping daemon's own directory"
        );
        assert!(
            fs.is_dir(&state_dir("d-22222222")).await,
            "used three days ago"
        );
        assert!(!fs.is_dir(&state_dir("workspace-12")).await);
    }

    #[gpui::test]
    async fn state_dirs_something_runs_in_are_kept_however_old(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        let long_ago = now() - STALE_STATE_DIR_AGE - DAY;
        // Another channel's daemon, running since long ago.
        state_dir_used_at(&fs, "d-33333333", long_ago, &[("daemon.pid", "333")]).await;
        // A server from before daemons, still running.
        state_dir_used_at(&fs, "workspace-4", long_ago, &[("server.pid", "44\n")]).await;
        // A proxy is starting a daemon here right now.
        state_dir_used_at(&fs, "d-55555555", long_ago, &[("daemon.lock", "")]).await;
        // A dead daemon's pid file.
        state_dir_used_at(&fs, "d-66666666", long_ago, &[("daemon.pid", "666")]).await;

        let outcome = sweep(&fs, &[333, 44], &["d-55555555"]).await;

        assert_eq!(outcome.removed_state_dirs, [state_dir("d-66666666")]);
        for kept in ["d-33333333", "workspace-4", "d-55555555"] {
            assert!(fs.is_dir(&state_dir(kept)).await, "{kept} is in use");
        }
    }

    #[gpui::test]
    async fn a_state_dir_counts_as_used_when_anything_in_it_changed(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        let long_ago = now() - STALE_STATE_DIR_AGE - DAY;
        state_dir_used_at(&fs, "d-77777777", long_ago, &[("daemon.lock", "")]).await;
        fs.set_next_mtime(now() - DAY);
        fs.insert_file(state_dir("d-77777777").join("daemon.sock"), Vec::new())
            .await;

        let outcome = sweep(&fs, &[], &[]).await;

        assert_eq!(outcome.removed_state_dirs, Vec::<PathBuf>::new());
    }

    #[gpui::test]
    async fn only_the_newest_logs_of_gone_daemons_are_kept(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        for generation in 0..8u32 {
            let written_at = now() - (10 + generation) * DAY;
            log_written_at(
                &fs,
                &format!("server-d-a000000{generation}.log"),
                written_at,
                10,
            )
            .await;
            log_written_at(
                &fs,
                &format!("server-d-a000000{generation}.1.log"),
                written_at,
                10,
            )
            .await;
        }
        log_written_at(&fs, "Zed.log", now() - 100 * DAY, 10).await;

        let outcome = sweep(&fs, &[], &[]).await;

        let mut removed = outcome.removed_logs;
        removed.sort();
        assert_eq!(
            removed,
            [
                log("server-d-a0000005.1.log"),
                log("server-d-a0000005.log"),
                log("server-d-a0000006.1.log"),
                log("server-d-a0000006.log"),
                log("server-d-a0000007.1.log"),
                log("server-d-a0000007.log"),
            ]
        );
        assert!(fs.is_file(&log("Zed.log")).await, "not a server log");
        assert!(fs.is_file(&log("server-d-a0000004.log")).await);
    }

    #[gpui::test]
    async fn logs_of_running_or_recent_daemons_are_kept_beyond_the_limits(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        let long_ago = now() - STALE_STATE_DIR_AGE - DAY;
        // The sweeping daemon's own log, oldest of all.
        log_written_at(&fs, "server-d-00000000.log", now() - 90 * DAY, 10).await;
        // Another channel's daemon that has been quiet for months.
        state_dir_used_at(&fs, "d-bbbbbbbb", long_ago, &[("daemon.pid", "777")]).await;
        log_written_at(&fs, "server-d-bbbbbbbb.log", now() - 80 * DAY, 10).await;
        // Written an hour ago: a daemon that may not have written its pid yet.
        log_written_at(
            &fs,
            "server-d-cccccccc.log",
            now() - Duration::from_secs(3600),
            10,
        )
        .await;
        for generation in 0..6u32 {
            log_written_at(
                &fs,
                &format!("server-d-e000000{generation}.log"),
                now() - (10 + generation) * DAY,
                10,
            )
            .await;
        }

        let outcome = sweep(&fs, &[777], &[]).await;

        let mut removed = outcome.removed_logs;
        removed.sort();
        assert_eq!(
            removed,
            [log("server-d-e0000004.log"), log("server-d-e0000005.log"),]
        );
        for kept in [
            "server-d-00000000.log",
            "server-d-bbbbbbbb.log",
            "server-d-cccccccc.log",
        ] {
            assert!(fs.is_file(&log(kept)).await, "{kept} must be kept");
        }
    }

    #[gpui::test]
    async fn old_logs_go_first_once_the_kept_ones_are_too_big(cx: &mut TestAppContext) {
        let fs = setup(cx).await;
        let megabyte = 1024 * 1024;
        log_written_at(&fs, "server-d-f0000000.log", now() - 10 * DAY, 8 * megabyte).await;
        log_written_at(&fs, "server-d-f0000001.log", now() - 11 * DAY, 8 * megabyte).await;
        log_written_at(&fs, "server-d-f0000002.log", now() - 12 * DAY, 8 * megabyte).await;
        log_written_at(&fs, "server-d-f0000003.log", now() - 13 * DAY, 8 * megabyte).await;

        let outcome = sweep(&fs, &[], &[]).await;

        let mut removed = outcome.removed_logs;
        removed.sort();
        assert_eq!(
            removed,
            [log("server-d-f0000002.log"), log("server-d-f0000003.log")]
        );
    }

    #[test]
    fn log_owner_covers_both_halves_of_a_rotated_log() {
        assert_eq!(
            log_owner("server-d-0123abcd.log"),
            Some("d-0123abcd".to_string())
        );
        assert_eq!(
            log_owner("server-d-0123abcd.1.log"),
            Some("d-0123abcd".to_string())
        );
        assert_eq!(
            log_owner("server-workspace-12.log"),
            Some("workspace-12".to_string())
        );
        assert_eq!(log_owner("Zed.log"), None::<String>);
        assert_eq!(log_owner("server-.log"), None);
        assert_eq!(log_owner("server-d-0123abcd.txt"), None);
    }

    #[test]
    fn a_held_lock_file_is_seen_as_held() {
        let temp_dir = tempfile::tempdir().expect("create a temporary directory");
        let lock_path = temp_dir.path().join(LOCK_FILE_NAME);
        assert!(!lock_file_is_held(&lock_path), "a missing lock file");
        let file = std::fs::File::create(&lock_path).expect("create the lock file");
        assert!(!lock_file_is_held(&lock_path));
        file.lock().expect("take the lock");
        assert!(lock_file_is_held(&lock_path));
        file.unlock().expect("release the lock");
        assert!(!lock_file_is_held(&lock_path));
    }
}
