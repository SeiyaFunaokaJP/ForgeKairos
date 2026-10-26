use anyhow::{Context as _, Result};
use std::process::Stdio;

/// A wrapper around `smol::process::Child` that attempts to clean up subprocess
/// trees: on Unix by using process groups, and on Windows by using job objects.
///
/// On Windows, killing or dropping this struct terminates the child together
/// with every process started under it that can still be traced back to it.
/// The job object also covers the Zed process exiting for any reason
/// (including crashes), since the OS then closes its handles and kills
/// whatever is still in the job. That last guarantee doesn't reach processes
/// outside the job: those an MSIX-packaged program (such as the Microsoft
/// Store build of PowerShell) starts, which Windows creates outside every job,
/// those started with `CREATE_BREAKAWAY_FROM_JOB`, and those the child starts
/// before it is assigned to the job. They are only found when the child is
/// killed or dropped. If the job can't be set up, the direct child remains
/// usable and only that walk is left.
pub struct Child {
    process: smol::process::Child,
    #[cfg(windows)]
    tree: Option<windows_job::ProcessTree>,
}

impl std::ops::Deref for Child {
    type Target = smol::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.process
    }
}

impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process
    }
}

impl Child {
    #[cfg(not(windows))]
    pub fn spawn(
        mut command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        crate::set_pre_exec_to_start_new_session(&mut command);
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;
        Ok(Self { process })
    }

    #[cfg(windows)]
    pub fn spawn(
        command: std::process::Command,
        stdin: Stdio,
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<Self> {
        let mut command = smol::process::Command::from(command);
        let process = command
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to spawn command {}",
                    crate::redact::redact_command(&format!("{command:?}"))
                )
            })?;

        // Assign the child to a job object configured to kill the entire
        // process tree when the last job handle is closed, so descendants
        // (e.g. node workers and MCP servers spawned by agent servers) are
        // reaped even if the direct child doesn't clean them up. Any process
        // the child spawns after this assignment is automatically part of the
        // job, unless the child is an MSIX-packaged program or asks to break
        // away.
        //
        // There is a small race: descendants the child spawns between the
        // `spawn()` call returning and the assignment below escape the job.
        // Closing it fully would require creating the process suspended
        // (`CREATE_SUSPENDED`), assigning it, then resuming it, which the
        // std/smol process APIs don't support without reimplementing process
        // creation. The window is normally short, and descendants created in
        // it are not in the job, but anything outside the job is still found
        // by its parent process ID when the child is killed or dropped.
        let tree = windows_job::ProcessTree::new(process.id())
            .map_err(|error| {
                log::error!("failed to track the spawned process tree: {error:#}");
            })
            .ok();

        Ok(Self { process, tree })
    }

    /// Consumes the child, draining its stdout/stderr and waiting for it to
    /// exit, then returns the collected output.
    pub async fn output(self) -> Result<std::process::Output> {
        // NOTE: Keep `self` alive across this await, do not destructure it to
        // pull `process` out first. On Windows that drops the process tree
        // early, which kills the child before `output()` finishes collecting
        // its stdout/stderr.
        Ok(self.process.output().await?)
    }

    #[cfg(not(windows))]
    pub fn kill(&mut self) -> Result<()> {
        let pid = self.process.id();
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        Ok(())
    }

    #[cfg(windows)]
    pub fn kill(&mut self) -> Result<()> {
        if let Some(tree) = &self.tree {
            tree.terminate()
        } else {
            self.process.kill()?;
            Ok(())
        }
    }

    /// Returns job activity, or `None` when descendant cleanup is unavailable.
    ///
    /// The count includes descendants after the direct child exits, but not
    /// the processes outside the job (see [`Child`]). `None` means job setup
    /// failed, not that the process tree is empty.
    ///
    /// # Errors
    ///
    /// Returns an error if Windows cannot query the owned job's activity.
    #[cfg(windows)]
    pub fn has_running_processes(&self) -> Result<Option<bool>> {
        Ok(match &self.tree {
            Some(tree) => tree.has_running_processes()?,
            None => None,
        })
    }
}

#[cfg(windows)]
mod windows_job {
    use crate::ResultExt as _;
    use anyhow::{Context as _, Result};
    use collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, STILL_ACTIVE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
                JobObjectExtendedLimitInformation, QueryInformationJobObject,
                SetInformationJobObject, TerminateJobObject,
            },
            Threading::{
                GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS,
                PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
                TerminateProcess,
            },
        },
    };

    const TERMINATE_ACCESS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(
        PROCESS_QUERY_LIMITED_INFORMATION.0 | PROCESS_SET_QUOTA.0 | PROCESS_TERMINATE.0,
    );

    /// A bound on how many processes a walk holds open, for a tree that keeps
    /// growing faster than it can be walked.
    const MAX_PROCESSES: usize = 4096;

    /// How many times the walk looks again, after terminating what it found,
    /// for processes started while it ran.
    const MAX_RESCANS: usize = 4;

    struct OwnedHandle(HANDLE);

    // SAFETY: Kernel object handles can be used from any thread.
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) }.log_err();
        }
    }

    /// An open process. Holding its handle keeps Windows from reusing the
    /// process ID, so the ID names this process even after it exits.
    struct OwnedProcess {
        handle: OwnedHandle,
        pid: u32,
        creation_time: u64,
        can_terminate: bool,
    }

    impl OwnedProcess {
        /// Opens a process with the rights to terminate it or, failing that,
        /// only to query it. A process opened that way can't be terminated,
        /// but its pinned ID still leads to the processes it started.
        fn open(pid: u32) -> Result<Self> {
            let (handle, can_terminate) = match unsafe { OpenProcess(TERMINATE_ACCESS, false, pid) }
            {
                Ok(handle) => (handle, true),
                Err(_) => {
                    let handle =
                        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                            .with_context(|| format!("failed to open process {pid}"))?;
                    (handle, false)
                }
            };
            let mut process = Self {
                handle: OwnedHandle(handle),
                pid,
                creation_time: 0,
                can_terminate,
            };
            process.creation_time = process.query_creation_time()?;
            Ok(process)
        }

        fn query_creation_time(&self) -> Result<u64> {
            let mut creation_time = FILETIME::default();
            let mut exit_time = FILETIME::default();
            let mut kernel_time = FILETIME::default();
            let mut user_time = FILETIME::default();
            unsafe {
                GetProcessTimes(
                    self.handle.0,
                    &mut creation_time,
                    &mut exit_time,
                    &mut kernel_time,
                    &mut user_time,
                )
            }
            .with_context(|| format!("failed to query times of process {}", self.pid))?;
            Ok((u64::from(creation_time.dwHighDateTime) << 32)
                | u64::from(creation_time.dwLowDateTime))
        }

        fn is_running(&self) -> bool {
            let mut exit_code = 0u32;
            unsafe { GetExitCodeProcess(self.handle.0, &mut exit_code) }.is_ok()
                && exit_code == STILL_ACTIVE.0 as u32
        }

        fn terminate(&self) -> Result<()> {
            if !self.can_terminate {
                return Ok(());
            }
            match unsafe { TerminateProcess(self.handle.0, 1) } {
                Ok(()) => Ok(()),
                // `TerminateProcess` fails with access denied once the
                // process has exited, which is the common case when a tree is
                // dropped, and can race with it exiting on its own.
                Err(_) if !self.is_running() => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("failed to terminate process {}", self.pid))
                }
            }
        }
    }

    /// A Win32 job object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
    /// all processes in the job are terminated when the last handle to the job
    /// is closed, which happens when this struct is dropped, or when the OS
    /// closes the owning process's handles after it exits for any reason.
    struct JobObject(OwnedHandle);

    impl JobObject {
        fn new() -> Result<Self> {
            unsafe {
                let job = Self(OwnedHandle(
                    CreateJobObjectW(None, None).context("failed to create job object")?,
                ));
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                // Without `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, a program in the
                // job that starts a process with `CREATE_BREAKAWAY_FROM_JOB`
                // fails with access denied instead of starting it.
                info.BasicLimitInformation.LimitFlags =
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                SetInformationJobObject(
                    job.0.0,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("failed to set job object limits")?;
                Ok(job)
            }
        }

        fn assign(&self, process: &OwnedProcess) -> Result<()> {
            unsafe { AssignProcessToJobObject(self.0.0, process.handle.0) }
                .with_context(|| format!("failed to assign process {} to job object", process.pid))
        }

        fn terminate(&self) -> Result<()> {
            unsafe { TerminateJobObject(self.0.0, 1).context("failed to terminate job object") }
        }

        fn has_running_processes(&self) -> Result<bool> {
            let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
            // SAFETY: self owns a valid job handle, and accounting is writable
            // for the exact buffer size passed to this synchronous query.
            unsafe {
                QueryInformationJobObject(
                    Some(self.0.0),
                    JobObjectBasicAccountingInformation,
                    &mut accounting as *mut _ as *mut _,
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    None,
                )
                .context("failed to query active job processes")?;
            }
            Ok(accounting.ActiveProcesses > 0)
        }
    }

    /// A spawned process and every process started under it.
    pub(super) struct ProcessTree {
        root: OwnedProcess,
        job: Option<JobObject>,
        terminated: AtomicBool,
    }

    impl ProcessTree {
        pub(super) fn new(pid: u32) -> Result<Self> {
            let root = OwnedProcess::open(pid)?;
            anyhow::ensure!(root.can_terminate, "not allowed to terminate process {pid}");
            let job = JobObject::new()
                .and_then(|job| {
                    job.assign(&root)?;
                    Ok(job)
                })
                .map_err(|error| {
                    log::error!("failed to assign spawned process to a job object: {error:#}");
                })
                .ok();
            Ok(Self {
                root,
                job,
                terminated: AtomicBool::new(false),
            })
        }

        /// Terminates the tree. Once that has succeeded, later calls,
        /// including the one when the tree is dropped, do nothing.
        pub(super) fn terminate(&self) -> Result<()> {
            if self.terminated.load(Ordering::Acquire) {
                return Ok(());
            }
            // The job doesn't hold everything under the root. Windows creates
            // the processes an MSIX-packaged program starts outside every
            // job, and the Microsoft Store build of PowerShell is such a
            // program: as the system shell wrapping an MCP or agent server's
            // command, the server it runs would survive `TerminateJobObject`.
            // Processes started with `CREATE_BREAKAWAY_FROM_JOB` are outside
            // it too. Everything under the root is found and held open before
            // anything is terminated, because a process may exit along with
            // its parent, and what it started would then no longer lead back
            // here.
            let root_was_running = self.root.is_running();
            let mut walk = Walk::new(&self.root);
            walk.extend();
            // Terminating the root first stops it from starting anything
            // more.
            let root_result = self.root.terminate();
            for process in &walk.reached {
                process.terminate().log_err();
            }
            // What was started while the walk ran names a process it holds.
            // Nothing can have been if the root had already exited and the
            // walk found nothing under it.
            let rescans = if root_was_running || !walk.reached.is_empty() {
                MAX_RESCANS
            } else {
                0
            };
            for _ in 0..rescans {
                let reached_before = walk.reached.len();
                if walk.extend() == 0 {
                    break;
                }
                for process in walk.reached.iter().skip(reached_before) {
                    process.terminate().log_err();
                }
            }
            let job_result = self.job.as_ref().map_or(Ok(()), JobObject::terminate);
            let result = root_result.and(job_result);
            if result.is_ok() {
                self.terminated.store(true, Ordering::Release);
            }
            result
        }

        /// Whether the job still holds a running process, or `None` when the
        /// root could not be assigned to one.
        pub(super) fn has_running_processes(&self) -> Result<Option<bool>> {
            self.job
                .as_ref()
                .map(JobObject::has_running_processes)
                .transpose()
        }

        #[cfg(test)]
        pub(super) fn job_contains(&self, pid: u32) -> bool {
            use windows::{Win32::System::JobObjects::IsProcessInJob, core::BOOL};

            let (Some(job), Ok(process)) = (&self.job, OwnedProcess::open(pid)) else {
                return false;
            };
            let mut contains = BOOL::default();
            unsafe { IsProcessInJob(process.handle.0, Some(job.0.0), &mut contains) }.is_ok()
                && contains.as_bool()
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            self.terminate().log_err();
        }
    }

    /// The processes found under a root, by the parent process ID each was
    /// created with. Every one stays open until the walk is dropped, which
    /// keeps its ID from being given to another process, so the processes it
    /// started keep naming it even once it has exited.
    struct Walk {
        reached: Vec<OwnedProcess>,
        creation_times: HashMap<u32, u64>,
        /// Whether the walk stopped at `MAX_PROCESSES`.
        full: bool,
    }

    impl Walk {
        fn new(root: &OwnedProcess) -> Self {
            let mut creation_times = HashMap::default();
            creation_times.insert(root.pid, root.creation_time);
            Self {
                reached: Vec::new(),
                creation_times,
                full: false,
            }
        }

        /// Opens the processes started by the root or by a process already
        /// reached that aren't reached yet, level by level until a level adds
        /// nothing, and returns how many were added. Once the walk holds
        /// `MAX_PROCESSES`, it adds no more.
        fn extend(&mut self) -> usize {
            if self.full {
                return 0;
            }
            let reached_before = self.reached.len();
            let mut snapshot = match process_parent_ids() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    log::error!("failed to list processes: {error:#}");
                    return 0;
                }
            };
            'levels: loop {
                // A process that exited since the snapshot can't be opened,
                // and has nothing left to terminate. It isn't remembered, so
                // a process later given its ID is looked at afresh.
                let opened = snapshot
                    .iter()
                    .filter(|&(pid, parent_pid)| {
                        self.creation_times.contains_key(parent_pid)
                            && !self.creation_times.contains_key(pid)
                    })
                    .filter_map(|(&pid, &parent_pid)| {
                        OwnedProcess::open(pid)
                            .ok()
                            .map(|process| (process, parent_pid))
                    })
                    .collect::<Vec<_>>();
                if opened.is_empty() {
                    break;
                }

                // Between the snapshot and opening, a process may have exited
                // and its ID gone to an unrelated process. Now that the open
                // handles pin the IDs, the next snapshot shows whose child
                // each one is, and it also lists the level below.
                snapshot = match process_parent_ids() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        log::error!("failed to list processes: {error:#}");
                        break;
                    }
                };
                let mut added = false;
                for (process, parent_pid) in opened {
                    let Some(&parent_creation_time) = self.creation_times.get(&parent_pid) else {
                        continue;
                    };
                    if !is_child_of(
                        process.pid,
                        process.creation_time,
                        parent_pid,
                        parent_creation_time,
                        &snapshot,
                    ) {
                        continue;
                    }
                    if self.reached.len() >= MAX_PROCESSES {
                        log::warn!("stopped walking a process tree at {MAX_PROCESSES} processes");
                        self.full = true;
                        break 'levels;
                    }
                    self.creation_times
                        .insert(process.pid, process.creation_time);
                    self.reached.push(process);
                    added = true;
                }
                if !added {
                    break;
                }
            }
            self.reached.len() - reached_before
        }
    }

    /// Whether an open process, listed as a child of `parent_pid` by a
    /// snapshot taken before it was opened, is a child of the process that
    /// holds that ID.
    ///
    /// Its entry in `current_parent_ids`, taken after it was opened, is its
    /// own, since the open handle keeps its ID from being reused. A parent ID
    /// is recorded when a process is created, so a process older than the
    /// current holder of that ID was started by an earlier holder. An equal
    /// creation time is accepted: creation times advance with the system
    /// clock's tick, and a child started right away can share its parent's.
    /// That leaves one case unnoticed: a process started by the ID's earlier
    /// holder in the same tick in which that holder exited and the ID went to
    /// the parent.
    pub(super) fn is_child_of(
        pid: u32,
        creation_time: u64,
        parent_pid: u32,
        parent_creation_time: u64,
        current_parent_ids: &HashMap<u32, u32>,
    ) -> bool {
        creation_time >= parent_creation_time && current_parent_ids.get(&pid) == Some(&parent_pid)
    }

    /// Maps every process on the system to the parent process ID it was
    /// created with.
    fn process_parent_ids() -> Result<HashMap<u32, u32>> {
        let snapshot = OwnedHandle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
                .context("failed to snapshot processes")?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        unsafe { Process32FirstW(snapshot.0, &mut entry) }
            .context("failed to read process snapshot")?;
        let mut parent_ids = HashMap::default();
        loop {
            parent_ids.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            // Fails with `ERROR_NO_MORE_FILES` past the last entry.
            if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
                break;
            }
        }
        Ok(parent_ids)
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[test]
    fn test_unmanaged_child_reports_unavailable_job_activity() {
        let mut command = gpui_util::new_std_command("ping.exe");
        command.args(["-n", "60", "127.0.0.1"]);
        let process = smol::process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn unmanaged process");
        let mut child = Child {
            process,
            tree: None,
        };

        let pid = child.id();
        assert!(
            process_is_alive(pid),
            "direct child should still be running"
        );
        assert_eq!(
            child.has_running_processes().expect("failed to query job"),
            None
        );
        child
            .kill()
            .expect("failed to kill unassigned direct child");
        smol::block_on(child.status()).expect("failed to reap direct child");
        assert_process_exits(pid, "direct child should be terminated");
    }

    #[test]
    fn test_job_tracks_grandchild_after_direct_child_exits() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("grandchild_pid");
        let mut command = gpui_util::new_std_command("powershell.exe");
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id",
            pid_file.display()
        ));
        let mut child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .expect("failed to spawn powershell");
        let status = smol::block_on(child.status()).expect("failed to await powershell");
        assert!(status.success(), "powershell failed: {status}");

        let grandchild_pid = std::fs::read_to_string(&pid_file)
            .expect("powershell did not record grandchild pid")
            .trim()
            .parse::<u32>()
            .expect("invalid grandchild pid");
        assert!(process_is_alive(grandchild_pid));
        assert_eq!(
            child.has_running_processes().expect("failed to query job"),
            Some(true),
            "job should remain active after its direct child exits"
        );

        child.kill().expect("failed to terminate job");
        let deadline = Instant::now() + Duration::from_secs(2);
        while child.has_running_processes().expect("failed to query job") != Some(false) {
            assert!(Instant::now() < deadline, "job did not become inactive");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_process_exits(grandchild_pid, "job grandchild should be terminated");
    }

    /// A PowerShell command that starts `ping` as a separate process, writes
    /// its pid to `pid_file`, and waits for it.
    fn start_grandchild_command(shell: &Path, pid_file: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(shell);
        command.args(["-NoProfile", "-Command"]).arg(format!(
            "$p = Start-Process -FilePath ping.exe -ArgumentList @('-n','60','127.0.0.1') -PassThru -WindowStyle Hidden; \
             Set-Content -LiteralPath '{}' -Value $p.Id; \
             Wait-Process -Id $p.Id",
            pid_file.display()
        ));
        command
    }

    fn wait_for_pid(pid_file: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        let pid = loop {
            if let Ok(contents) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = contents.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            process_is_alive(pid),
            "grandchild should be alive after spawning"
        );
        pid
    }

    /// Spawns a process tree `powershell -> ping` via `Child::spawn` and
    /// returns the `Child` along with the pid of the grandchild (`ping`).
    fn spawn_process_tree(shell: &Path, temp_dir: &Path) -> (Child, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let child = Child::spawn(
            start_grandchild_command(shell, &pid_file),
            Stdio::null(),
            Stdio::null(),
            Stdio::null(),
        )
        .expect("failed to spawn powershell");
        (child, wait_for_pid(&pid_file))
    }

    /// The Microsoft Store build of PowerShell, which Zed picks as the system
    /// shell when there is no PowerShell under Program Files.
    fn packaged_pwsh() -> Option<PathBuf> {
        let windows_apps =
            PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Microsoft\\WindowsApps");
        std::fs::read_dir(windows_apps)
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("Microsoft.PowerShell_")
            })
            .map(|entry| entry.path().join("pwsh.exe"))
            .find(|path| path.exists())
    }

    fn process_is_alive(pid: u32) -> bool {
        use windows::Win32::{
            Foundation::{CloseHandle, STILL_ACTIVE},
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut exit_code = 0u32;
            let alive = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                && exit_code == STILL_ACTIVE.0 as u32;
            CloseHandle(handle).expect("failed to close process handle");
            alive
        }
    }

    fn assert_process_exits(pid: u32, message: &str) {
        // `TerminateProcess` returns before the process is gone.
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_is_alive(pid) {
            assert!(Instant::now() < deadline, "{message} (pid {pid})");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) =
            spawn_process_tree(Path::new("powershell.exe"), temp_dir.path());

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after killing the child",
        );
    }

    #[test]
    fn test_drop_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (child, grandchild_pid) =
            spawn_process_tree(Path::new("powershell.exe"), temp_dir.path());

        drop(child);

        assert_process_exits(
            grandchild_pid,
            "grandchild should be terminated after dropping the child",
        );
    }

    /// Starts `powershell -> ping` outside any job of ours, then tracks the
    /// tree. Assigning the root to a job after its child exists leaves the
    /// child outside the job, as Windows does for the children of an
    /// MSIX-packaged program.
    fn track_tree_with_grandchild_outside_the_job(
        temp_dir: &Path,
    ) -> (smol::process::Child, windows_job::ProcessTree, u32) {
        let pid_file = temp_dir.join("grandchild_pid");
        let root = smol::process::Command::from(start_grandchild_command(
            Path::new("powershell.exe"),
            &pid_file,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn powershell");
        let grandchild_pid = wait_for_pid(&pid_file);
        let tree = windows_job::ProcessTree::new(root.id()).expect("failed to track the tree");
        assert!(
            !tree.job_contains(grandchild_pid),
            "grandchild should have been started outside the job"
        );
        (root, tree, grandchild_pid)
    }

    #[test]
    fn test_terminate_reaches_grandchildren_outside_the_job() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut root, tree, grandchild_pid) =
            track_tree_with_grandchild_outside_the_job(temp_dir.path());

        tree.terminate().expect("failed to terminate the tree");

        assert_process_exits(
            grandchild_pid,
            "grandchild outside the job should be terminated with the tree",
        );
        smol::block_on(root.status()).expect("failed to wait for powershell");
    }

    #[test]
    fn test_drop_reaches_grandchildren_outside_the_job() {
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut root, tree, grandchild_pid) =
            track_tree_with_grandchild_outside_the_job(temp_dir.path());

        drop(tree);

        assert_process_exits(
            grandchild_pid,
            "grandchild outside the job should be terminated when the tree is dropped",
        );
        smol::block_on(root.status()).expect("failed to wait for powershell");
    }

    #[test]
    fn test_terminate_reaches_the_bottom_of_a_deep_chain_outside_the_job() {
        use std::os::windows::process::CommandExt as _;

        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("deepest_pid");
        // The root and five more levels of `cmd`, then PowerShell writing its
        // pid six levels below the root.
        let mut command = std::process::Command::new("cmd.exe");
        command.raw_arg(format!(
            "/d /c {}powershell.exe -NoProfile -Command \"Set-Content -LiteralPath '{}' -Value $PID; Start-Sleep -Seconds 60\"",
            "cmd.exe /d /c ".repeat(5),
            pid_file.display()
        ));
        let mut root = smol::process::Command::from(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn cmd");
        let deepest_pid = wait_for_pid(&pid_file);
        // Joining the root to a job only now leaves the whole chain outside it.
        let tree = windows_job::ProcessTree::new(root.id()).expect("failed to track the tree");
        assert!(
            !tree.job_contains(deepest_pid),
            "the chain should have been started outside the job"
        );

        tree.terminate().expect("failed to terminate the tree");

        assert_process_exits(
            deepest_pid,
            "the bottom of a chain outside the job should be terminated with the tree",
        );
        smol::block_on(root.status()).expect("failed to wait for cmd");
    }

    #[test]
    fn test_kill_terminates_what_packaged_powershell_started() {
        let Some(pwsh) = packaged_pwsh() else {
            eprintln!("skipping: the Microsoft Store build of PowerShell is not installed");
            return;
        };
        let temp_dir = tempfile::tempdir().unwrap();
        let (mut child, grandchild_pid) = spawn_process_tree(&pwsh, temp_dir.path());
        let tree = child
            .tree
            .as_ref()
            .expect("the spawned process should be tracked");
        assert!(
            !tree.job_contains(grandchild_pid),
            "packaged PowerShell should have started the grandchild outside the job"
        );

        child.kill().expect("failed to kill child");

        assert_process_exits(
            grandchild_pid,
            "a process packaged PowerShell started should be terminated with it",
        );
    }

    #[test]
    fn test_parentage_is_checked_against_a_snapshot_taken_after_opening() {
        let parent_pid = 100;
        let parent_creation_time = 1_000;
        let current_parent_ids = [(200, parent_pid), (300, 999)]
            .into_iter()
            .collect::<collections::HashMap<u32, u32>>();

        assert!(windows_job::is_child_of(
            200,
            2_000,
            parent_pid,
            parent_creation_time,
            &current_parent_ids
        ));
        assert!(
            windows_job::is_child_of(
                200,
                parent_creation_time,
                parent_pid,
                parent_creation_time,
                &current_parent_ids
            ),
            "a child created in the same clock tick as its parent"
        );
        assert!(
            !windows_job::is_child_of(
                300,
                2_000,
                parent_pid,
                parent_creation_time,
                &current_parent_ids
            ),
            "the ID went to a process another parent started"
        );
        assert!(
            !windows_job::is_child_of(
                400,
                2_000,
                parent_pid,
                parent_creation_time,
                &current_parent_ids
            ),
            "the process exited and its ID is free"
        );
        assert!(
            !windows_job::is_child_of(
                200,
                500,
                parent_pid,
                parent_creation_time,
                &current_parent_ids
            ),
            "started by an earlier holder of the parent's ID"
        );
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn process_exists(pid: i32) -> bool {
        // Signal 0 only checks for existence. A killed grandchild is
        // reparented to init, which reaps it, so it stops existing.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn test_kill_terminates_grandchildren() {
        let temp_dir = tempfile::tempdir().unwrap();
        let pid_file = temp_dir.path().join("grandchild_pid");
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "sleep 600 & echo $! > '{}'; wait",
            pid_file.display()
        ));
        let mut child = Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .expect("failed to spawn sh");

        let deadline = Instant::now() + Duration::from_secs(10);
        let grandchild_pid = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = contents.trim().parse::<i32>()
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for grandchild pid file"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(process_exists(grandchild_pid));

        child.kill().expect("failed to kill child");

        let deadline = Instant::now() + Duration::from_secs(10);
        while process_exists(grandchild_pid) {
            assert!(
                Instant::now() < deadline,
                "grandchild should be terminated with the process group (pid {grandchild_pid})"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
