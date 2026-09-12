//! The **built-in protected set**: locations the sandbox keeps readable but
//! never writable, whatever the caller asked for.
//!
//! The caller only knows to protect the project's `.git` locations
//! (`agent::sandboxing::sandbox_git_dirs`), which silently assumes the worktree
//! does not *contain* the account's own secrets. It regularly does — this fork's
//! saved SSH connections open `/home/<user>` itself, and a local project can just
//! as easily be the home directory — and then everything below it is a writable
//! worktree: `~/.ssh/authorized_keys` (grant yourself a login), `~/.bashrc` (run
//! on the next login), `~/.zed_server/…` (overwrite the remote server binary),
//! this fork's own config directory (rewrite `settings.json` and the next launch
//! talks to whichever model endpoint the command chose), `~/.local/bin` and
//! `~/.cargo/bin` (binaries already on `PATH`), plus every credential store. A
//! sandbox that blocks the network but lets the command write those is not a
//! sandbox.
//!
//! So the *enforcement* layer adds them rather than trusting its caller's list:
//! [`crate::Sandbox::new`] for the local backends (Seatbelt / Bubblewrap /
//! WSL-Bubblewrap) and `linux_remote_helper::run_helper` for the SSH-remote
//! host. Both share [`UNIX_PROTECTED_HOME_RELATIVE`]; only the local side adds
//! the running application's own directories, which don't exist remotely. On
//! Windows there are two homes to cover — `%USERPROFILE%` and the WSL distro
//! account the sandbox actually runs in. `Sandbox::new` adds the Windows list;
//! the helper running inside WSL adds the Unix one for the distro account
//! (plus [`WSL_SANDBOX_HELPER_HOME_RELATIVE`]), as it does on an SSH host.
//!
//! Semantics, identical on both sides: **readable, never writable/creatable/
//! deletable**. An entry that does not exist yet must not be creatable either
//! (writing a missing `~/.bash_login` is as good as editing one): Seatbelt
//! denies by path, and the WSL / SSH-remote helper denies new entries in the
//! directory that would hold it. Bubblewrap can only bind what exists, so it
//! skips them. An entry that exists but cannot be captured/pinned fails the
//! command closed. Both directories and plain files appear here.
//!
//! `/etc` and the rest of the system directories need no entry: they are outside
//! any worktree, and no backend grants write outside the worktree.

use std::path::{Path, PathBuf};

/// Paths — relative to the account's home directory — that are protected on
/// **every** Unix run.
///
/// Shared verbatim with the SSH-remote helper, whose behavior on real hosts was
/// validated against exactly this list; treat additions as a change that needs
/// re-validation there, and put local-only entries in
/// [`LOCAL_EXTRA_PROTECTED_HOME_RELATIVE`] instead. On Windows it is applied by
/// the helper inside WSL, not by this process.
#[cfg(not(target_os = "windows"))]
pub(crate) const UNIX_PROTECTED_HOME_RELATIVE: &[&str] = &[
    // Credentials and keys.
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".config/gh",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    // This fork's own remote install: the server binary the next connection
    // executes, and the state directory beside it.
    ".zed_server",
    ".local/share/zed-jp-enhance",
    // Binaries already on `PATH` for the next interactive shell.
    ".cargo/bin",
    ".local/bin",
    // Shell init: written once, executed on every later login.
    ".bashrc",
    ".bash_aliases",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".cshrc",
    ".tcshrc",
    ".login",
    ".config/fish",
    ".bash_logout",
    // Read by `pam_env` at login where `user_readenv` is on, so it can set
    // `LD_PRELOAD` for every later session.
    ".pam_environment",
];

/// The startup files zsh reads from `$ZDOTDIR` instead of the home directory
/// when that is set.
#[cfg(target_os = "linux")]
const ZSH_STARTUP_FILES: &[&str] = &[".zshenv", ".zprofile", ".zshrc", ".zlogin", ".zlogout"];

/// The Windows equivalent of [`UNIX_PROTECTED_HOME_RELATIVE`], relative to
/// `%USERPROFILE%`.
///
/// Not a translation of the Unix list but the same *threat* list spelled the way
/// Windows spells it: the credential stores Windows tools actually use, the
/// startup surfaces a Windows login runs, this fork's `%APPDATA%` /
/// `%LOCALAPPDATA%` directories, and `.wslconfig` — which configures the WSL VM
/// the Windows sandbox itself runs inside (it can name a replacement kernel, so
/// a write there is a sandbox escape on the next WSL restart). The Git-for-
/// Windows shell init files are included because that shell is what a sandboxed
/// command typically runs under.
#[cfg(target_os = "windows")]
const WINDOWS_PROTECTED_HOME_RELATIVE: &[&str] = &[
    // Credentials and keys.
    r".ssh",
    r".gnupg",
    r"AppData\Roaming\gnupg",
    r".aws",
    r".azure",
    r".kube",
    r".docker\config.json",
    r".config\gh",
    r"AppData\Roaming\GitHub CLI",
    r".netrc",
    // `_netrc` is the spelling curl and friends look for on Windows.
    r"_netrc",
    r".git-credentials",
    // This fork's own config/data/state: `settings.json` (rewrite it and the
    // next launch talks to whichever model endpoint the command chose), the
    // thread database, and the logs.
    r"AppData\Roaming\Zed-JP-Enhance",
    r"AppData\Local\Zed-JP-Enhance",
    // Binaries already on `PATH` for the next shell.
    r".cargo\bin",
    r".local\bin",
    // Shell init: written once, executed on every later session. PowerShell
    // reads its profiles from `Documents`; the rest are Git for Windows' bash.
    r"Documents\PowerShell",
    r"Documents\WindowsPowerShell",
    r".bashrc",
    r".bash_aliases",
    r".bash_profile",
    r".bash_login",
    r".profile",
    r".bash_logout",
    // Run at every logon — the Windows analogue of `.bashrc`.
    r"AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup",
    // Configures the WSL VM the Windows sandbox runs inside.
    r".wslconfig",
];

/// The home-relative entries the **local** sandbox protects on this platform:
/// the Unix list on Unix, its Windows counterpart on Windows.
#[cfg(target_os = "windows")]
const LOCAL_PROTECTED_HOME_RELATIVE: &[&str] = WINDOWS_PROTECTED_HOME_RELATIVE;
#[cfg(not(target_os = "windows"))]
const LOCAL_PROTECTED_HOME_RELATIVE: &[&str] = UNIX_PROTECTED_HOME_RELATIVE;

/// Entries only the **local** sandbox adds, on top of
/// [`LOCAL_PROTECTED_HOME_RELATIVE`]: the running application's own
/// config/state directories (its settings, thread database and logs), and the
/// per-platform login-time execution surfaces.
///
/// These are deliberately absent from the remote helper's set: the remote host
/// runs `remote_server`, not the editor, so it has no such directories, and the
/// helper's list is frozen against its on-host validation.
///
/// The default locations are home-relative and listed here; a user who moved
/// them with `XDG_*` / `%APPDATA%` is covered by
/// [`app_state_directories_from_environment`].
#[cfg(target_os = "linux")]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[
    ".config/zed-jp-enhance",
    ".local/state/zed-jp-enhance",
    ".config/autostart",
    ".config/systemd/user",
];

#[cfg(target_os = "macos")]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[
    "Library/Application Support/Zed-JP-Enhance",
    ".config/zed-jp-enhance",
    ".local/state/Zed-JP-Enhance",
    "Library/LaunchAgents",
];

/// Windows keeps its application directories in the main list above (they are
/// plain `AppData` subdirectories), so there is nothing extra to add.
#[cfg(target_os = "windows")]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[];

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[];

/// This fork's directory name inside a config/data/state base directory. Must
/// match `paths::APP_NAME` / `paths::APP_NAME_LOWERCASE`, which is where the
/// running editor actually keeps `settings.json`, the thread database and the
/// logs. Duplicated rather than depended upon so this crate — which is also the
/// static musl remote-server helper — keeps its very small dependency surface.
/// `pub` (re-exported as [`crate::APP_DIRECTORY_NAME`]) so a drift-detection
/// test can compare it against `paths::APP_NAME`; that test lives in the
/// `agent` crate, which already depends on both `paths` and `sandbox`.
pub const APP_DIRECTORY_NAME: &str = if cfg!(target_os = "linux") {
    "zed-jp-enhance"
} else {
    "Zed-JP-Enhance"
};

/// Resolve `relative_paths` against `home`. A trailing separator on `home` is
/// absorbed by `Path::join`, so `/home/u` and `/home/u/` yield the same paths.
fn resolve_home_relative(home: &Path, relative_paths: &[&str]) -> Vec<PathBuf> {
    relative_paths
        .iter()
        .map(|relative| home.join(relative))
        .collect()
}

/// The set the **SSH-remote helper** protects: exactly
/// [`UNIX_PROTECTED_HOME_RELATIVE`], resolved against the remote account's home
/// directory.
#[cfg(target_os = "linux")]
pub(crate) fn remote_builtin_protected_paths(home: &Path) -> Vec<PathBuf> {
    resolve_home_relative(home, UNIX_PROTECTED_HOME_RELATIVE)
}

/// Append the remote built-in protected paths to the client's list, dropping
/// ones the client already named (which would otherwise be pinned twice).
#[cfg(target_os = "linux")]
pub(crate) fn merge_remote_builtin_protected_paths(
    requested: Vec<PathBuf>,
    home: &Path,
) -> Vec<PathBuf> {
    let mut merged = requested;
    for builtin in remote_builtin_protected_paths(home) {
        if !merged.contains(&builtin) {
            merged.push(builtin);
        }
    }
    merged
}

/// The set the **local** sandbox protects: the shared list plus this platform's
/// local-only entries, plus any application directory the environment moved
/// away from its home-relative default.
///
/// `app_state_directories` comes from [`app_state_directories_from_environment`];
/// it is a parameter so the resolution itself stays a pure function.
pub(crate) fn local_builtin_protected_paths(
    home: &Path,
    app_state_directories: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = resolve_home_relative(home, LOCAL_PROTECTED_HOME_RELATIVE);
    paths.extend(resolve_home_relative(
        home,
        LOCAL_EXTRA_PROTECTED_HOME_RELATIVE,
    ));
    for directory in app_state_directories {
        if !paths.contains(directory) {
            paths.push(directory.clone());
        }
    }
    paths
}

/// Where the Windows backend installs its WSL sandbox helper (the bundled Linux
/// `remote_server` and `slirp4netns`), relative to the distro account's home.
///
/// Inside WSL the helper itself protects the distro account's home — the Unix
/// list, exactly as on an SSH host — and in WSL mode this directory as well:
/// overwriting the binary that runs before every sandboxed command would be
/// code execution outside the sandbox (the WSL counterpart of `.zed_server`).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_SANDBOX_HELPER_HOME_RELATIVE: &str = ".cache/zed-jp-enhance";
/// The account's home directory, read from the environment.
///
/// Every process that runs a sandboxed command inherits it, whether it is the
/// editor started from a desktop session or the remote helper started from the
/// user's own ssh session.
///
/// Returning `None` is fatal to sandbox construction: with no home directory the
/// built-in protected set cannot be determined, and running anyway would be
/// running without it.
#[cfg(target_os = "linux")]
pub(crate) fn home_dir_from_environment() -> Option<PathBuf> {
    // The passwd fallback covers the rare non-login invocation with `$HOME`
    // unset, where the account still has a home directory to protect.
    absolute_env_path("HOME").or_else(home_dir_from_passwd)
}

/// The account's home directory from the account database alone, for callers
/// that cannot trust their environment: the sandbox helper, whose environment
/// can come from the Windows side (`WSLENV`) or a dev container's
/// `containerEnv`.
#[cfg(target_os = "linux")]
pub(crate) fn home_dir_from_account() -> Option<PathBuf> {
    home_dir_from_passwd()
}

/// zsh's startup files in `$ZDOTDIR`, when the environment names an absolute
/// one. Taking this from the environment is safe: it can only add protection.
#[cfg(target_os = "linux")]
pub(crate) fn zsh_startup_files_from_environment() -> Vec<PathBuf> {
    absolute_env_path("ZDOTDIR")
        .map(|zdotdir| resolve_home_relative(&zdotdir, ZSH_STARTUP_FILES))
        .unwrap_or_default()
}

/// See the Linux [`home_dir_from_environment`]. macOS has no passwd fallback
/// because this crate takes no `libc` dependency there.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn home_dir_from_environment() -> Option<PathBuf> {
    absolute_env_path("HOME")
}

/// See the Linux [`home_dir_from_environment`].
#[cfg(target_os = "windows")]
pub(crate) fn home_dir_from_environment() -> Option<PathBuf> {
    absolute_env_path("USERPROFILE").or_else(|| {
        // A domain-joined account can have `%USERPROFILE%` unset while the
        // legacy pair still names the profile.
        let mut home = PathBuf::from(std::env::var_os("HOMEDRIVE")?);
        home.push(PathBuf::from(std::env::var_os("HOMEPATH")?));
        home.is_absolute().then_some(home)
    })
}

/// The value of environment variable `name` as an absolute path, or `None` when
/// it is unset or relative. A relative value would be resolved against this
/// process's working directory, silently protecting an unpredictable location.
fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

/// `getpwuid(geteuid())->pw_dir`: the home directory of the account this
/// process acts as.
#[cfg(target_os = "linux")]
fn home_dir_from_passwd() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    // The reentrant variant: the editor calls this from `Sandbox::new` on
    // whatever thread wraps the command, and the non-`_r` `getpwuid` hands back
    // a process-wide static buffer that any concurrent passwd lookup (a shell
    // spawn, a git operation) could overwrite underneath us.
    let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut entry: *mut libc::passwd = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 1024];
    loop {
        // SAFETY: `passwd`, `buffer` and `entry` are valid for the whole call and
        // `buffer.len()` is the true capacity; glibc and musl both fill `passwd`
        // with pointers into `buffer`, which outlives every read below.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                &mut passwd,
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut entry,
            )
        };
        if status == libc::ERANGE && buffer.len() < 64 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || entry.is_null() || passwd.pw_dir.is_null() {
            return None;
        }
        break;
    }
    // SAFETY: `pw_dir` is a NUL-terminated string inside `buffer`, still alive.
    let dir = unsafe { std::ffi::CStr::from_ptr(passwd.pw_dir) };
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    home.is_absolute().then_some(home)
}

/// The directories this fork keeps its own config/data/state in, as the
/// environment names them.
///
/// [`local_builtin_protected_paths`] already covers the home-relative defaults;
/// this catches the case where the user (or a roaming profile) moved a base
/// directory elsewhere, which would otherwise leave `settings.json` writable.
/// Only absolute bases are used; the returned paths may not exist, and are
/// filtered by the same existence rule as the rest of the set.
pub(crate) fn app_state_directories_from_environment() -> Vec<PathBuf> {
    // macOS is absent on purpose: `paths::{config_dir, data_dir, state_dir}`
    // there are fixed home-relative locations and consult no environment
    // variable, so the defaults above already cover them.
    #[cfg(target_os = "linux")]
    let base_variables: &[&str] = &["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"];
    #[cfg(target_os = "windows")]
    let base_variables: &[&str] = &["APPDATA", "LOCALAPPDATA"];
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let base_variables: &[&str] = &[];

    let mut directories = Vec::new();
    for variable in base_variables {
        let Some(base) = absolute_env_path(variable) else {
            continue;
        };
        let directory = base.join(APP_DIRECTORY_NAME);
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is resolved against the home directory, including the
    /// multi-segment and plain-file entries, and a trailing separator on the
    /// home directory changes nothing.
    #[test]
    fn local_builtin_paths_resolve_against_home() {
        let home = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        };
        let paths = local_builtin_protected_paths(&home, &[]);

        for entry in LOCAL_PROTECTED_HOME_RELATIVE
            .iter()
            .chain(LOCAL_EXTRA_PROTECTED_HOME_RELATIVE)
        {
            assert!(
                paths.contains(&home.join(entry)),
                "{entry} must always be protected"
            );
        }
        assert!(
            paths.iter().all(|path| path.starts_with(&home)),
            "every entry of the default set is home-relative"
        );

        let mut with_separator = home.as_os_str().to_os_string();
        with_separator.push(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            local_builtin_protected_paths(Path::new(&with_separator), &[]),
            paths,
            "a trailing separator on the home directory must not change the paths"
        );
    }

    /// The credential stores, the shell init files and this fork's own
    /// directories are all present on the platform being built for — the three
    /// classes the set exists to cover.
    #[test]
    fn local_builtin_paths_cover_each_threat_class() {
        let home = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        };
        let paths = local_builtin_protected_paths(&home, &[]);
        let contains = |relative: &str| paths.contains(&home.join(relative));

        assert!(contains(".ssh"), "ssh keys");
        assert!(contains(".git-credentials"), "git credential store");
        assert!(contains(".cargo/bin"), "binaries already on PATH");
        if cfg!(target_os = "windows") {
            assert!(contains(r"Documents\PowerShell"), "shell init");
            assert!(
                contains(r"AppData\Roaming\Zed-JP-Enhance")
                    || contains(r"AppData\Local\Zed-JP-Enhance"),
                "this fork's own settings/state directory"
            );
            assert!(contains(".wslconfig"), "the sandbox's own WSL substrate");
        } else {
            assert!(contains(".bashrc"), "shell init");
            assert!(
                contains(".config/zed-jp-enhance")
                    || contains("Library/Application Support/Zed-JP-Enhance"),
                "this fork's own settings/state directory"
            );
        }
    }

    /// An application directory the environment moved away from its
    /// home-relative default is added, and one that is already covered is not
    /// duplicated.
    #[test]
    fn moved_application_directories_are_added_once() {
        let home = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        };
        let default_relative = if cfg!(target_os = "windows") {
            r"AppData\Roaming\Zed-JP-Enhance"
        } else if cfg!(target_os = "macos") {
            "Library/Application Support/Zed-JP-Enhance"
        } else {
            ".config/zed-jp-enhance"
        };
        let moved = if cfg!(target_os = "windows") {
            PathBuf::from(r"D:\state\Zed-JP-Enhance")
        } else {
            PathBuf::from("/var/state/zed-jp-enhance")
        };

        let baseline = local_builtin_protected_paths(&home, &[]);
        let paths = local_builtin_protected_paths(
            &home,
            &[moved.clone(), home.join(default_relative), moved.clone()],
        );

        assert!(
            paths.contains(&moved),
            "a moved directory must be protected"
        );
        assert_eq!(
            paths.iter().filter(|path| *path == &moved).count(),
            1,
            "a moved directory must be added exactly once"
        );
        assert_eq!(
            paths.len(),
            baseline.len() + 1,
            "an already-covered directory must not be added again"
        );
    }

    /// Every entry is a relative path: an absolute one would silently ignore the
    /// home directory and protect something else entirely.
    #[test]
    fn every_entry_is_relative() {
        for entry in LOCAL_PROTECTED_HOME_RELATIVE
            .iter()
            .chain(LOCAL_EXTRA_PROTECTED_HOME_RELATIVE)
        {
            let entry = Path::new(entry);
            assert!(entry.is_relative(), "{} must be relative", entry.display());
            assert!(
                !entry.components().any(|component| matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
                "{} must not contain `.` or `..`",
                entry.display()
            );
        }
    }
}
