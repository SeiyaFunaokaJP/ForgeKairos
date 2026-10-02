use std::{
    ffi::{OsStr, OsString},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::RestartManager::{
            CCH_RM_SESSION_KEY, RmEndSession, RmGetList, RmRegisterResources, RmShutdown,
            RmStartSession,
        },
        UI::WindowsAndMessaging::PostMessageW,
    },
    core::{PCWSTR, PWSTR},
};

use crate::windows_impl::WM_JOB_UPDATED;

pub(crate) struct Job {
    pub apply: Box<dyn Fn(&Path) -> Result<()> + Send + Sync>,
    pub rollback: Box<dyn Fn(&Path) -> Result<()> + Send + Sync>,
}

impl Job {
    pub fn mkdir(name: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                std::fs::create_dir_all(&dir)
                    .context(format!("Failed to create directory {}", dir.display()))
            }),
            rollback: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                std::fs::remove_dir_all(&dir)
                    .context(format!("Failed to remove directory {}", dir.display()))
            }),
        }
    }

    pub fn mkdir_if_exists(name: &'static Path, check: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let dir = app_dir.join(name);
                let check = app_dir.join(check);

                if check.exists() {
                    std::fs::create_dir_all(&dir)
                        .context(format!("Failed to create directory {}", dir.display()))?
                }
                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let dir = app_dir.join(name);

                if dir.exists() {
                    std::fs::remove_dir_all(&dir)
                        .context(format!("Failed to remove directory {}", dir.display()))?
                }

                Ok(())
            }),
        }
    }

    pub fn move_file(filename: &'static Path, new_filename: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);
                log::info!(
                    "Moving file: {}->{}",
                    old_file.display(),
                    new_file.display()
                );

                std::fs::rename(&old_file, new_file)
                    .context(format!("Failed to move file {}", old_file.display()))
            }),
            rollback: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);
                log::info!(
                    "Rolling back file move: {}->{}",
                    old_file.display(),
                    new_file.display()
                );

                std::fs::rename(&new_file, &old_file).context(format!(
                    "Failed to rollback file move {}->{}",
                    new_file.display(),
                    old_file.display()
                ))
            }),
        }
    }

    pub fn move_if_exists(filename: &'static Path, new_filename: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);

                if old_file.exists() {
                    log::info!(
                        "Moving file: {}->{}",
                        old_file.display(),
                        new_file.display()
                    );

                    std::fs::rename(&old_file, new_file)
                        .context(format!("Failed to move file {}", old_file.display()))?;
                }

                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let old_file = app_dir.join(filename);
                let new_file = app_dir.join(new_filename);

                if new_file.exists() {
                    log::info!(
                        "Rolling back file move: {}->{}",
                        old_file.display(),
                        new_file.display()
                    );

                    std::fs::rename(&new_file, &old_file).context(format!(
                        "Failed to rollback file move {}->{}",
                        new_file.display(),
                        old_file.display()
                    ))?
                }

                Ok(())
            }),
        }
    }

    pub fn rmdir_nofail(filename: &'static Path) -> Self {
        Job {
            apply: Box::new(move |app_dir| {
                let filename = app_dir.join(filename);
                log::info!("Removing file: {}", filename.display());
                if let Err(e) = std::fs::remove_dir_all(&filename) {
                    log::warn!("Failed to remove directory: {}", e);
                }

                Ok(())
            }),
            rollback: Box::new(move |app_dir| {
                let filename = app_dir.join(filename);
                anyhow::bail!(
                    "Delete operations cannot be rolled back, file: {}",
                    filename.display()
                )
            }),
        }
    }
}

/// The executable the installer puts in the app directory, which is launched
/// once the update is in place.
const EXECUTABLE: &str = branding::concat_const!(branding::CURRENT.executable_stem, ".exe");

/// The files an update replaces, relative to the app directory: the
/// executable, the command-line launcher and its shell script. The names an
/// earlier build of this fork installed under follow, because the build being
/// updated may be one of those and its files have to be moved out of the way
/// just the same.
fn files_replaced_by_update() -> Vec<String> {
    replaced_files(std::iter::once(&branding::CURRENT).chain(branding::LEGACY))
}

fn replaced_files<'a>(name_sets: impl IntoIterator<Item = &'a branding::Names>) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for names in name_sets {
        for file in [
            format!("{}.exe", names.executable_stem),
            format!("bin\\{}.exe", names.cli_name),
            format!("bin\\{}", names.cli_name),
        ] {
            // Windows file names are case-insensitive, so a name that differs
            // only in case is the same file and must be moved only once.
            if !files.iter().any(|known| known.eq_ignore_ascii_case(&file)) {
                files.push(file);
            }
        }
    }
    files
}

/// What else the installer puts in `bin` (`"$innoDir\bin\..."` in
/// `script/bundle-windows.ps1`): the remote servers and slirp4netns uploaded to
/// remote hosts, and clang-format. An update stages them in `install\bin` like
/// the launchers; left there, they would be deleted with it, and the previous
/// release's remote server, whose version no longer matches the client's, would
/// keep being uploaded.
const BUNDLED_BIN_FILES: [&str; 5] = [
    "bin\\zed-remote-server-windows-x86_64.zip",
    "bin\\zed-remote-server-windows-aarch64.zip",
    "bin\\zed-remote-server-linux-x86_64.gz",
    "bin\\slirp4netns-linux-x86_64",
    "bin\\clang-format.exe",
];

/// The helper runs once per update and exits, so the few paths built from
/// `branding` at runtime are leaked to get the `'static` lifetime [`Job`] takes.
fn leak(path: String) -> &'static Path {
    Box::leak(PathBuf::from(path).into_boxed_path())
}

fn update_jobs() -> Vec<Job> {
    fn p(value: &str) -> &Path {
        Path::new(value)
    }
    let cli = format!("bin\\{}.exe", branding::CURRENT.cli_name);
    let cli_script = format!("bin\\{}", branding::CURRENT.cli_name);
    let mut jobs = vec![
        // Move old files
        // Not deleting because installing new files can fail
        Job::mkdir(p("old")),
        Job::mkdir(p("old\\bin")),
    ];
    // A build from before the rename has its own names instead of the
    // current ones, so none of them is required to exist.
    jobs.extend(files_replaced_by_update().into_iter().map(|file| {
        let moved_to = leak(format!("old\\{file}"));
        Job::move_if_exists(leak(file), moved_to)
    }));
    // A file the new release no longer ships goes too, as it would be absent
    // from a fresh install of it.
    jobs.extend(
        BUNDLED_BIN_FILES
            .into_iter()
            .map(|file| Job::move_if_exists(Path::new(file), leak(format!("old\\{file}")))),
    );
    jobs.extend([
        //
        // TODO: remove after a few weeks once everyone is on the new version and this file never exists
        Job::move_if_exists(p("OpenConsole.exe"), p("old\\OpenConsole.exe")),
        Job::mkdir(p("old\\x64")),
        Job::mkdir(p("old\\arm64")),
        Job::move_if_exists(p("x64\\OpenConsole.exe"), p("old\\x64\\OpenConsole.exe")),
        Job::move_if_exists(
            p("arm64\\OpenConsole.exe"),
            p("old\\arm64\\OpenConsole.exe"),
        ),
        //
        Job::move_file(p("conpty.dll"), p("old\\conpty.dll")),
        // Copy new files
        Job::move_file(leak(format!("install\\{EXECUTABLE}")), p(EXECUTABLE)),
        Job::move_file(leak(format!("install\\{cli}")), leak(cli)),
        Job::move_file(leak(format!("install\\{cli_script}")), leak(cli_script)),
    ]);
    jobs.extend(
        BUNDLED_BIN_FILES
            .into_iter()
            .map(|file| Job::move_if_exists(leak(format!("install\\{file}")), Path::new(file))),
    );
    jobs.extend([
        //
        Job::mkdir_if_exists(p("x64"), p("install\\x64")),
        Job::mkdir_if_exists(p("arm64"), p("install\\arm64")),
        Job::move_if_exists(
            p("install\\x64\\OpenConsole.exe"),
            p("x64\\OpenConsole.exe"),
        ),
        Job::move_if_exists(
            p("install\\arm64\\OpenConsole.exe"),
            p("arm64\\OpenConsole.exe"),
        ),
        //
        Job::move_file(p("install\\conpty.dll"), p("conpty.dll")),
        // Cleanup installer and updates folder
        Job::rmdir_nofail(p("updates")),
        Job::rmdir_nofail(p("install")),
        // Cleanup old installation
        Job::rmdir_nofail(p("old")),
    ]);
    jobs
}

#[cfg(not(test))]
pub(crate) static JOBS: LazyLock<Vec<Job>> = LazyLock::new(update_jobs);

#[cfg(test)]
pub(crate) static JOBS: LazyLock<[Job; 9]> = LazyLock::new(|| {
    fn p(value: &str) -> &Path {
        Path::new(value)
    }
    [
        Job {
            apply: Box::new(|_| {
                std::thread::sleep(Duration::from_millis(1000));
                if let Ok(config) = std::env::var("ZED_AUTO_UPDATE") {
                    match config.as_str() {
                        "err1" => Err(std::io::Error::other("Simulated error")).context("Anyhow!"),
                        "err2" => Ok(()),
                        _ => panic!("Unknown ZED_AUTO_UPDATE value: {}", config),
                    }
                } else {
                    Ok(())
                }
            }),
            rollback: Box::new(|_| {
                unsafe { std::env::set_var("ZED_AUTO_UPDATE_RB", "rollback1") };
                Ok(())
            }),
        },
        Job::mkdir(p("test1")),
        Job::mkdir_if_exists(p("test_exists"), p("test1")),
        Job::mkdir_if_exists(p("test_missing"), p("dont")),
        Job {
            apply: Box::new(|folder| {
                std::fs::write(folder.join("test1/test"), "test")?;
                Ok(())
            }),
            rollback: Box::new(|folder| {
                std::fs::remove_file(folder.join("test1/test"))?;
                Ok(())
            }),
        },
        Job::move_file(p("test1/test"), p("test1/moved")),
        Job::move_if_exists(p("test1/test"), p("test1/noop")),
        Job {
            apply: Box::new(|_| {
                std::thread::sleep(Duration::from_millis(1000));
                if let Ok(config) = std::env::var("ZED_AUTO_UPDATE") {
                    match config.as_str() {
                        "err1" => Ok(()),
                        "err2" => Err(std::io::Error::other("Simulated error")).context("Anyhow!"),
                        _ => panic!("Unknown ZED_AUTO_UPDATE value: {}", config),
                    }
                } else {
                    Ok(())
                }
            }),
            rollback: Box::new(|_| Ok(())),
        },
        Job::rmdir_nofail(p("test1/nofolder")),
    ]
});

/// Attempts to use Windows Restart Manager to release file handles held by other processes
/// (e.g., Explorer.exe) on the files we need to move during the update.
///
/// This is a best-effort operation - if it fails, we'll still try the update and rely on
/// the retry logic.
fn release_file_handles(app_dir: &Path) -> Result<()> {
    // Files that commonly get locked by Explorer or other processes
    let files_to_release: Vec<PathBuf> = files_replaced_by_update()
        .into_iter()
        .chain(["conpty.dll".to_string()])
        .map(|file| app_dir.join(file))
        .collect();

    log::info!("Attempting to release file handles using Restart Manager...");

    let mut session: u32 = 0;
    let mut session_key = [0u16; CCH_RM_SESSION_KEY as usize + 1];

    // Start a Restart Manager session
    let err = unsafe {
        RmStartSession(
            &mut session,
            Some(0),
            PWSTR::from_raw(session_key.as_mut_ptr()),
        )
    };
    if err.is_err() {
        anyhow::bail!("RmStartSession failed: {err:?}");
    }

    // Ensure we end the session when done
    let _session_guard = scopeguard::guard(session, |s| {
        let _ = unsafe { RmEndSession(s) };
    });

    // Convert paths to wide strings for Windows API
    let wide_paths: Vec<Vec<u16>> = files_to_release
        .iter()
        .filter(|p| p.exists())
        .map(|p| {
            OsStr::new(p)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        })
        .collect();

    if wide_paths.is_empty() {
        log::info!("No files to release handles for");
        return Ok(());
    }

    let pcwstr_paths: Vec<PCWSTR> = wide_paths
        .iter()
        .map(|p| PCWSTR::from_raw(p.as_ptr()))
        .collect();

    // Register the files we want to modify
    let err = unsafe { RmRegisterResources(session, Some(&pcwstr_paths), None, None) };
    if err.is_err() {
        anyhow::bail!("RmRegisterResources failed: {err:?}");
    }

    // Check if any processes are using these files
    let mut needed: u32 = 0;
    let mut count: u32 = 0;
    let mut reboot_reasons: u32 = 0;
    let _ = unsafe { RmGetList(session, &mut needed, &mut count, None, &mut reboot_reasons) };

    if needed == 0 {
        log::info!("No processes are holding handles to the files");
        return Ok(());
    }

    log::info!(
        "{} process(es) are holding handles to the files, requesting release...",
        needed
    );

    // Request processes to release their handles
    // RmShutdown with flags=0 asks applications to release handles gracefully
    // For Explorer, this typically releases icon cache handles without closing Explorer
    let err = unsafe { RmShutdown(session, 0, None) };
    if err.is_err() {
        anyhow::bail!("RmShutdown failed: {:?}", err);
    }

    log::info!("Successfully requested handle release");
    Ok(())
}

#[allow(clippy::disallowed_methods, reason = "doesn't run in the main binary")]
fn zed_launch_command(app_dir: &Path, launch_arguments: &[OsString]) -> std::process::Command {
    let mut command = std::process::Command::new(app_dir.join(EXECUTABLE));
    command.args(launch_arguments);
    command
}

pub(crate) fn perform_update(
    app_dir: &Path,
    hwnd: Option<isize>,
    launch: bool,
    launch_arguments: &[OsString],
) -> Result<()> {
    let hwnd = hwnd.map(|ptr| HWND(ptr as _));

    // Try to release file handles before starting the update
    if let Err(e) = release_file_handles(app_dir) {
        log::warn!("Restart Manager failed (will continue anyway): {}", e);
    }

    let mut last_successful_job = None;
    'outer: for (i, job) in JOBS.iter().enumerate() {
        let start = Instant::now();
        loop {
            if start.elapsed().as_secs() > 2 {
                log::error!("Timed out, rolling back");
                break 'outer;
            }
            match (job.apply)(app_dir) {
                Ok(_) => {
                    last_successful_job = Some(i);
                    unsafe { PostMessageW(hwnd, WM_JOB_UPDATED, WPARAM(0), LPARAM(0))? };
                    break;
                }
                Err(err) => match err.downcast_ref::<std::io::Error>() {
                    Some(io_err) => match io_err.kind() {
                        std::io::ErrorKind::NotFound => {
                            log::error!("Operation failed with file not found, aborting: {}", err);
                            break 'outer;
                        }
                        _ => {
                            log::error!("Operation failed (retrying): {}", err);
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    },
                    None => {
                        log::error!("Operation failed with unexpected error, aborting: {}", err);
                        break 'outer;
                    }
                },
            }
        }
    }

    if last_successful_job
        .map(|job| job != JOBS.len() - 1)
        .unwrap_or(true)
    {
        let Some(last_successful_job) = last_successful_job else {
            anyhow::bail!("Autoupdate failed, nothing to rollback");
        };

        for job in (0..=last_successful_job).rev() {
            let job = &JOBS[job];
            if let Err(e) = (job.rollback)(app_dir) {
                anyhow::bail!(
                    "Job rollback failed, the app might be left in an inconsistent state: ({:?})",
                    e
                );
            }
        }

        anyhow::bail!("Autoupdate failed, rollback successful");
    }

    if launch {
        #[allow(clippy::disallowed_methods, reason = "doesn't run in the main binary")]
        let _child = zed_launch_command(app_dir, launch_arguments)
            .spawn()
            .context("Failed to launch Zed after update")?;
    }
    log::info!("Update completed successfully");
    Ok(())
}

#[cfg(test)]
mod test {
    use std::{ffi::OsString, path::Path};

    use super::{
        EXECUTABLE, files_replaced_by_update, perform_update, replaced_files, update_jobs,
        zed_launch_command,
    };

    #[test]
    fn test_zed_launch_command_preserves_arguments() {
        let arguments = vec![
            OsString::from("--user-data-dir"),
            OsString::from(r"C:\Zed Data"),
        ];
        let command = zed_launch_command(Path::new(r"C:\Program Files\Zed"), &arguments);

        assert_eq!(
            command.get_program(),
            Path::new(r"C:\Program Files\Zed")
                .join(EXECUTABLE)
                .as_os_str()
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            arguments
                .iter()
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_perform_update() {
        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        assert!(perform_update(app_dir, None, false, &[]).is_ok());

        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        // Simulate a timeout
        unsafe { std::env::set_var("ZED_AUTO_UPDATE", "err1") };
        let ret = perform_update(app_dir, None, false, &[]);
        assert!(
            ret.is_err_and(|e| e.to_string().as_str() == "Autoupdate failed, nothing to rollback")
        );

        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        // Simulate a timeout
        unsafe { std::env::set_var("ZED_AUTO_UPDATE", "err2") };
        let ret = perform_update(app_dir, None, false, &[]);
        assert!(
            ret.is_err_and(|e| e.to_string().as_str() == "Autoupdate failed, rollback successful")
        );
        assert!(std::env::var("ZED_AUTO_UPDATE_RB").is_ok_and(|e| e == "rollback1"));
    }

    fn write_file(app_dir: &Path, file: &str, contents: &str) {
        let path = app_dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// What the installer leaves in `install\` for the helper to move in.
    fn write_staged_update(app_dir: &Path) {
        let cli_name = branding::CURRENT.cli_name;
        write_file(app_dir, &format!("install\\{EXECUTABLE}"), "new");
        write_file(app_dir, &format!("install\\bin\\{cli_name}.exe"), "new");
        write_file(app_dir, &format!("install\\bin\\{cli_name}"), "new");
        write_file(app_dir, "install\\conpty.dll", "new");
        write_file(app_dir, "conpty.dll", "old");
    }

    fn apply_update_jobs(app_dir: &Path) {
        for job in update_jobs() {
            (job.apply)(app_dir).unwrap();
        }
    }

    fn assert_updated(app_dir: &Path) {
        let cli_name = branding::CURRENT.cli_name;
        for file in [
            EXECUTABLE.to_string(),
            format!("bin\\{cli_name}.exe"),
            format!("bin\\{cli_name}"),
            "conpty.dll".to_string(),
        ] {
            assert_eq!(
                std::fs::read_to_string(app_dir.join(&file)).unwrap(),
                "new",
                "{file}"
            );
        }
        for directory in ["install", "old"] {
            assert!(
                !app_dir.join(directory).exists(),
                "{directory} is left behind"
            );
        }
    }

    #[test]
    fn an_update_replaces_the_current_files() {
        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        let cli_name = branding::CURRENT.cli_name;
        write_file(app_dir, EXECUTABLE, "old");
        write_file(app_dir, &format!("bin\\{cli_name}.exe"), "old");
        write_file(app_dir, &format!("bin\\{cli_name}"), "old");
        write_staged_update(app_dir);

        apply_update_jobs(app_dir);

        assert_updated(app_dir);
    }

    #[test]
    fn an_update_from_a_build_before_the_rename_moves_its_files_out() {
        for legacy in branding::LEGACY {
            let app_dir = tempfile::tempdir().unwrap();
            let app_dir = app_dir.path();
            let legacy_files = [
                format!("{}.exe", legacy.executable_stem),
                format!("bin\\{}.exe", legacy.cli_name),
                format!("bin\\{}", legacy.cli_name),
            ];
            for file in &legacy_files {
                write_file(app_dir, file, "old");
            }
            write_staged_update(app_dir);

            apply_update_jobs(app_dir);

            assert_updated(app_dir);
            for file in &legacy_files {
                assert!(!app_dir.join(file).exists(), "{file} is left behind");
            }
        }
    }

    /// Every file `script/bundle-windows.ps1` puts in the installer's `bin`,
    /// for each architecture it builds.
    fn files_the_installer_puts_in_bin() -> Vec<String> {
        let script_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/bundle-windows.ps1");
        let script = std::fs::read_to_string(&script_path).unwrap();
        let mut files: Vec<String> = Vec::new();
        for rest in script.split("\"$innoDir\\bin\\").skip(1) {
            let (name, _) = rest.split_once('"').unwrap();
            for architecture in ["x86_64", "aarch64"] {
                let file = format!("bin\\{}", name.replace("$Architecture", architecture));
                if !files.contains(&file) {
                    files.push(file);
                }
            }
        }
        files
    }

    #[test]
    fn an_update_replaces_everything_the_installer_puts_in_bin() {
        let files = files_the_installer_puts_in_bin();
        assert!(
            files.len() > 3,
            "bundle-windows.ps1 puts more than the launchers in bin: {files:?}"
        );
        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        write_staged_update(app_dir);
        for file in &files {
            write_file(app_dir, file, "old");
            write_file(app_dir, &format!("install\\{file}"), "new");
        }

        apply_update_jobs(app_dir);

        assert_updated(app_dir);
        for file in &files {
            assert_eq!(
                std::fs::read_to_string(app_dir.join(file)).unwrap(),
                "new",
                "{file} is installed to bin but not replaced by an update"
            );
        }
    }

    #[test]
    fn a_bundled_file_the_update_does_not_ship_is_removed() {
        let app_dir = tempfile::tempdir().unwrap();
        let app_dir = app_dir.path();
        write_staged_update(app_dir);
        write_file(app_dir, "bin\\slirp4netns-linux-x86_64", "old");

        apply_update_jobs(app_dir);

        assert_updated(app_dir);
        assert!(!app_dir.join("bin\\slirp4netns-linux-x86_64").exists());
    }

    #[test]
    fn each_replaced_file_is_moved_once() {
        let current = branding::CURRENT;
        let differing_only_in_case = branding::Names {
            executable_stem: "FORGEKAIROS-CASE",
            cli_name: "forgekairos-case",
            ..current
        };
        let same_file_in_other_case = branding::Names {
            executable_stem: "forgekairos-case",
            cli_name: "FORGEKAIROS-CASE",
            ..current
        };
        let files = replaced_files([&differing_only_in_case, &same_file_in_other_case]);
        assert_eq!(files.len(), 3, "{files:?}");

        let files = files_replaced_by_update();
        for (index, file) in files.iter().enumerate() {
            assert!(
                !files[..index]
                    .iter()
                    .any(|earlier| earlier.eq_ignore_ascii_case(file)),
                "{file} is listed twice; the second move would fail"
            );
        }
        assert!(files.iter().any(|file| file == EXECUTABLE));
    }
}
