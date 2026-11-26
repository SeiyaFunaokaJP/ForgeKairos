//! `remote.max_cpu_cores`: keeps the daemon, and the processes it starts
//! (language servers, git, node), on a number of CPU cores, so that a host
//! shared with other work is not saturated by indexing after a reconnect.
//!
//! The limit is a CPU affinity rather than smaller thread pools: an affinity is
//! inherited by every process the daemon starts, which is where most of the load
//! comes from, and the pools sized from the available parallelism afterwards
//! shrink with it. Terminals, tasks and external agents are started by the
//! client over their own connection, not by the daemon, so what runs in them
//! keeps every core. So does a process that detached from the daemon's tree
//! before the limit was set.

use gpui::App;

#[cfg(target_os = "linux")]
pub(crate) fn init(cx: &mut App) {
    use crate::RemoteHostSettings;
    use gpui::AppContext as _;
    use settings::{Settings as _, SettingsStore};
    use std::sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU32, Ordering},
    };

    let allowed = match linux::affinity_of(0) {
        Ok(allowed) => allowed,
        Err(error) => {
            log::warn!("remote.max_cpu_cores has no effect on this host: {error}");
            return;
        }
    };

    let latest_limit = Arc::new(AtomicU32::new(0));
    // One walk at a time, each for the limit that is current when it starts:
    // two walks running side by side could leave threads on the older limit.
    let walking = Arc::new(Mutex::new(()));
    cx.observe_global::<SettingsStore>(move |cx| {
        let limit = RemoteHostSettings::get_global(cx).max_cpu_cores;
        if latest_limit.swap(limit, Ordering::SeqCst) == limit {
            return;
        }
        let allowed = allowed.clone();
        let latest_limit = latest_limit.clone();
        let walking = walking.clone();
        cx.background_spawn(async move {
            let _walking = walking.lock().unwrap_or_else(PoisonError::into_inner);
            let limit = latest_limit.load(Ordering::SeqCst);
            let cores = linux::limited(&allowed, limit);
            let outcome = linux::confine_process_tree(std::process::id(), cores);
            if outcome.moved == 0 && outcome.refused > 0 {
                log::warn!(
                    "remote.max_cpu_cores = {limit} could not be applied: the kernel refused to move {} threads",
                    outcome.refused
                );
            } else {
                log::info!(
                    "running on {} of {} CPU cores (remote.max_cpu_cores = {limit}): moved {} threads, {} refused",
                    cores.len(),
                    allowed.len(),
                    outcome.moved,
                    outcome.refused
                );
            }
        })
        .detach();
    })
    .detach();
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn init(_cx: &mut App) {}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        collections::{HashMap, HashSet},
        io,
        path::Path,
    };

    #[derive(Debug)]
    pub(super) struct Outcome {
        pub moved: usize,
        /// Threads the kernel would not move, such as those of a process
        /// that runs as another user.
        pub refused: usize,
    }

    /// The cores of `allowed` to run on under `limit`: all of them without a
    /// limit (0), or with one the host cannot exceed anyway.
    pub(super) fn limited(allowed: &[usize], limit: u32) -> &[usize] {
        match usize::try_from(limit) {
            Ok(limit) if limit > 0 && limit < allowed.len() => &allowed[..limit],
            _ => allowed,
        }
    }

    /// The CPUs the thread `thread_id` may run on (0 for the calling thread).
    pub(super) fn affinity_of(thread_id: libc::pid_t) -> io::Result<Vec<usize>> {
        // SAFETY: a `cpu_set_t` is a plain bit mask, for which all zeroes is
        // the empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` outlives the call and is as large as we say.
        let result = unsafe {
            libc::sched_getaffinity(thread_id, std::mem::size_of::<libc::cpu_set_t>(), &mut set)
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((0..libc::CPU_SETSIZE as usize)
            // SAFETY: the index is below `CPU_SETSIZE`, the size of `set`.
            .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
            .collect())
    }

    fn set_affinity(thread_id: libc::pid_t, cores: &[usize]) -> io::Result<()> {
        // SAFETY: as in `affinity_of`.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for core in cores {
            if *core < libc::CPU_SETSIZE as usize {
                // SAFETY: the index is below `CPU_SETSIZE`, the size of `set`.
                unsafe { libc::CPU_SET(*core, &mut set) };
            }
        }
        // SAFETY: `set` outlives the call and is as large as we say.
        let result = unsafe {
            libc::sched_setaffinity(thread_id, std::mem::size_of::<libc::cpu_set_t>(), &set)
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Moves every thread of `root` and of the processes descended from it
    /// onto `cores`.
    pub(super) fn confine_process_tree(root: u32, cores: &[usize]) -> Outcome {
        // A thread or process started while a pass runs inherits the affinity
        // its parent had at that moment, so passes repeat until one finds
        // nothing left to move. Bounded, for a tree that never stops forking.
        const MAX_PASSES: usize = 5;

        let mut moved = 0;
        let mut refused = HashSet::new();
        for _ in 0..MAX_PASSES {
            let mut moved_in_this_pass = false;
            for process in process_tree(root) {
                for thread in
                    numeric_entries(&Path::new("/proc").join(process.to_string()).join("task"))
                {
                    let Ok(thread) = libc::pid_t::try_from(thread) else {
                        continue;
                    };
                    if affinity_of(thread).is_ok_and(|current| current == cores) {
                        continue;
                    }
                    match set_affinity(thread, cores) {
                        Ok(()) => {
                            moved += 1;
                            moved_in_this_pass = true;
                        }
                        // The thread ended in the meantime.
                        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                        Err(error) => {
                            if refused.insert(thread) {
                                log::debug!(
                                    "could not move thread {thread} of process {process}: {error}"
                                );
                            }
                        }
                    }
                }
            }
            if !moved_in_this_pass {
                break;
            }
        }
        Outcome {
            moved,
            refused: refused.len(),
        }
    }

    /// `root` and every running process descended from it.
    fn process_tree(root: u32) -> Vec<u32> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for process in numeric_entries(Path::new("/proc")) {
            let status = Path::new("/proc").join(process.to_string()).join("status");
            if let Some(parent) = std::fs::read_to_string(status)
                .ok()
                .and_then(|status| parent_in_status(&status))
            {
                children.entry(parent).or_default().push(process);
            }
        }
        let mut tree = vec![root];
        let mut next = 0;
        while let Some(process) = tree.get(next).copied() {
            next += 1;
            if let Some(children) = children.remove(&process) {
                tree.extend(children);
            }
        }
        tree
    }

    /// The parent's id in the contents of `/proc/<pid>/status`.
    pub(super) fn parent_in_status(status: &str) -> Option<u32> {
        status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|parent| parent.trim().parse().ok())
    }

    fn numeric_entries(directory: &Path) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{affinity_of, confine_process_tree, limited, parent_in_status};

    #[test]
    fn the_limit_keeps_the_first_cores_and_zero_keeps_all() {
        let allowed = [0, 1, 4, 5];
        assert_eq!(limited(&allowed, 0), &allowed);
        assert_eq!(limited(&allowed, 1), &[0]);
        assert_eq!(limited(&allowed, 3), &[0, 1, 4]);
        assert_eq!(limited(&allowed, 4), &allowed);
        assert_eq!(limited(&allowed, 64), &allowed);
        assert_eq!(limited(&[], 2), &[] as &[usize]);
    }

    #[test]
    fn the_parent_is_read_from_a_status_file() {
        let status = "Name:\tsleep (1)\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t812\nPid:\t812\nPPid:\t37\nTracerPid:\t0\n";
        assert_eq!(parent_in_status(status), Some(37));
        assert_eq!(parent_in_status("Name:\tsleep\n"), None);
        assert_eq!(parent_in_status("PPid:\tnone\n"), None);
    }

    /// Run in a child process of its own, because the affinity it changes is
    /// that of every thread of the process, other tests' included.
    #[test]
    fn a_limit_reaches_running_threads_and_children_and_is_lifted_again() {
        const ROLE: &str = "FORGEKAIROS_CPU_LIMIT_TEST_CHILD";
        if std::env::var_os(ROLE).is_none() {
            let name = concat!(
                module_path!(),
                "::a_limit_reaches_running_threads_and_children_and_is_lifted_again"
            );
            let name = name
                .split_once("::")
                .map_or(name, |(_crate_name, path)| path);
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", name, "--nocapture"])
                .env(ROLE, "1")
                .output()
                .expect("starting the test binary again");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the child test did not pass: {}
{stdout}
{stderr}",
                output.status
            );
            return;
        }

        let allowed = affinity_of(0).expect("reading the affinity");
        if allowed.len() < 2 {
            eprintln!("skipped: a single CPU leaves nothing to limit");
            return;
        }

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (thread_id_tx, thread_id_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            // SAFETY: `gettid` takes no arguments and cannot fail.
            let thread_id = unsafe { libc::syscall(libc::SYS_gettid) } as libc::pid_t;
            thread_id_tx.send(thread_id).expect("sending the thread id");
            stop_rx.recv().ok();
        });
        let thread_id = thread_id_rx.recv().expect("the thread's id");
        // A grandchild, to cover more than the direct children.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("starting a shell");
        let grandchild = {
            use std::io::BufRead as _;
            let mut line = String::new();
            std::io::BufReader::new(child.stdout.take().expect("the shell's output"))
                .read_line(&mut line)
                .expect("reading the grandchild's id");
            line.trim().parse::<libc::pid_t>().expect("a process id")
        };
        let child_id = libc::pid_t::try_from(child.id()).expect("a process id");

        let one = limited(&allowed, 1);
        let outcome = confine_process_tree(std::process::id(), one);
        assert_eq!(outcome.refused, 0, "{outcome:?}");
        assert!(outcome.moved >= 4, "{outcome:?}");
        for subject in [0, thread_id, child_id, grandchild] {
            assert_eq!(affinity_of(subject).expect("reading the affinity"), one);
        }
        // What starts under the limit inherits it.
        let inherited = std::thread::spawn(|| affinity_of(0).expect("reading the affinity"))
            .join()
            .expect("the thread");
        assert_eq!(inherited, one);

        let lifted = confine_process_tree(std::process::id(), limited(&allowed, 0));
        assert_eq!(lifted.refused, 0, "{lifted:?}");
        for subject in [0, thread_id, child_id, grandchild] {
            assert_eq!(affinity_of(subject).expect("reading the affinity"), allowed);
        }

        stop_tx.send(()).ok();
        thread.join().expect("the thread");
        // SAFETY: `grandchild` is the `sleep` this test started.
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
        child.kill().ok();
        child.wait().ok();
    }
}
