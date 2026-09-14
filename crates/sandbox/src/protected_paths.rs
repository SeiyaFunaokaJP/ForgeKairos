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
    // Git's global configuration, which the editor's own git reads outside the
    // sandbox: a filter, `core.hooksPath` or `core.fsmonitor` set there runs
    // on the host (ADR-F49).
    ".gitconfig",
    ".config/git",
    // Agent CLIs' user-level configuration, which an agent running on the host
    // reads: Claude Code's hooks, commands and MCP servers, Gemini CLI's and
    // Codex's (ADR-F49).
    ".claude.json",
    ".claude",
    ".gemini",
    ".codex",
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
    // Git's global configuration, which the editor's own git reads outside the
    // sandbox (ADR-F49).
    r".gitconfig",
    r".config\git",
    // Agent CLIs' user-level configuration, which an agent running on the host
    // reads (ADR-F49).
    r".claude.json",
    r".claude",
    r".gemini",
    r".codex",
    // Shims on `PATH`, which a Windows program started by name may run
    // (ADR-F49).
    r"AppData\Roaming\npm",
    r"AppData\Local\Microsoft\WindowsApps",
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

/// The locations the local sandbox always protects — readable, never
/// writable, whatever the caller asked for — for an account whose home
/// directory is `home` and a command that may write `worktree_roots`, exactly
/// as [`crate::Sandbox::new`] adds them: the credential stores, shell init
/// files, git's global configuration and the `PATH` binary and shim
/// directories under `home`; this fork's own configuration and state
/// directories, wherever the environment puts them; and in each worktree root
/// the agent configurations a host agent acts on
/// ([`WORKTREE_PROTECTED_RELATIVE`]). Some of them may not exist.
///
/// For code outside the sandbox that has to enforce the same rule, such as the
/// guard on an external agent's file writes, so that it uses this list rather
/// than a copy of it. `Sandbox::new` takes `home` from `HOME` (on Windows
/// `USERPROFILE`), falling back to the account database, and the worktree
/// roots from the directories it grants write access to.
pub fn builtin_protected_paths(home: &Path, worktree_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = local_builtin_protected_paths(home, &protected_directories_from_environment());
    for path in worktree_protected_paths(worktree_roots) {
        push_unique(&mut paths, path);
    }
    paths
}

/// Paths, relative to a worktree root, that a program on the *host* reads and
/// acts on: Claude Code's project directory and MCP servers, and Gemini CLI's
/// and Codex's project directories. The whole `.claude/` is protected, not only
/// `settings.json`: the host Claude Code also runs the hook scripts under
/// `.claude/hooks/` and the commands and skills beside them. A sandboxed
/// command writing any of these would configure — through hooks and MCP
/// servers, run — code outside the sandbox the next time a host agent opens the
/// project. So they are protected in every worktree root a command may write,
/// as its `.git` is (ADR-F49).
pub(crate) const WORKTREE_PROTECTED_RELATIVE: &[&str] =
    &[".claude", ".mcp.json", ".gemini", ".codex"];

/// [`WORKTREE_PROTECTED_RELATIVE`] resolved against each of `worktree_roots`.
pub(crate) fn worktree_protected_paths(worktree_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in worktree_roots {
        for relative in WORKTREE_PROTECTED_RELATIVE {
            let mut path = root.clone();
            // Component by component, so that it is spelled with this
            // platform's separators.
            path.extend(relative.split('/'));
            push_unique(&mut paths, path);
        }
    }
    paths
}

/// Git's configuration directory below `$XDG_CONFIG_HOME`, when that names an
/// absolute directory; its default, `~/.config/git`, is in the home-relative
/// lists.
pub(crate) fn git_config_directory_from_environment() -> Option<PathBuf> {
    absolute_env_path("XDG_CONFIG_HOME").map(|base| base.join("git"))
}

/// The protected locations the environment names beyond the home-relative
/// defaults: this fork's own directories below a moved base directory
/// ([`app_state_directories_from_environment`]), git's XDG configuration
/// directory, and on Windows the `PATH` shim directories of a moved
/// `%APPDATA%` / `%LOCALAPPDATA%`. Taking these from the environment is safe:
/// it can only add protection.
pub(crate) fn protected_directories_from_environment() -> Vec<PathBuf> {
    let mut directories = app_state_directories_from_environment();
    if let Some(git) = git_config_directory_from_environment() {
        push_unique(&mut directories, git);
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(roaming) = absolute_env_path("APPDATA") {
            push_unique(&mut directories, roaming.join("npm"));
        }
        if let Some(local) = absolute_env_path("LOCALAPPDATA") {
            push_unique(
                &mut directories,
                local.join("Microsoft").join("WindowsApps"),
            );
        }
    }
    directories
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// ADR-F49 decision 11: entries, relative to a home directory, whose *contents*
/// no sandboxed command may read, on any backend and whatever it asked for.
///
/// The protected set above only keeps writes out, and that is not enough for a
/// secret: a command that can `cat ~/.ssh/id_ed25519` puts the key in its
/// output, which goes to the model, and can copy it into the worktree for the
/// user to push. So these are the credential and key stores, keyrings and
/// browser profiles, the agent configurations (tokens in `.claude.json`, the
/// host IDE server's auth token in `.claude/ide`), and this fork's own
/// configuration, data and remote install. `/` separators; one that does not
/// exist is simply not hidden.
pub(crate) const READ_HIDDEN_HOME_RELATIVE: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".config/gh",
    ".npmrc",
    ".yarnrc.yml",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gcloud",
    ".terraform.d",
    ".local/share/keyrings",
    ".password-store",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".claude.json",
    ".claude",
    ".gemini",
    ".codex",
    ".config/zed-jp-enhance",
    ".local/share/zed-jp-enhance",
    ".local/state/zed-jp-enhance",
    ".zed_server",
];

/// What a Windows home hides on top of [`READ_HIDDEN_HOME_RELATIVE`]: `_netrc`,
/// the spelling curl looks for there, and all of `AppData`, where browser
/// profiles, application tokens and this fork's own data live.
pub(crate) const READ_HIDDEN_WINDOWS_HOME_EXTRA: &[&str] = &["_netrc", "AppData"];

/// macOS keeps this fork's data, and the browsers' and the system's secret
/// stores, outside the XDG-style locations above.
#[cfg(target_os = "macos")]
const LOCAL_READ_HIDDEN_EXTRA: &[&str] = &[
    "Library/Application Support/Zed-JP-Enhance",
    ".local/state/Zed-JP-Enhance",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/Firefox",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Arc",
    "Library/Application Support/Vivaldi",
    "Library/Cookies",
    "Library/Keychains",
    "Library/Safari",
    "Library/Containers/com.apple.Safari",
    "Library/Mail",
    "Library/Messages",
];
#[cfg(not(target_os = "macos"))]
const LOCAL_READ_HIDDEN_EXTRA: &[&str] = &[];

/// [`READ_HIDDEN_HOME_RELATIVE`] resolved against `home`, together with
/// [`READ_HIDDEN_WINDOWS_HOME_EXTRA`] when `home` is a Windows home.
pub(crate) fn read_hidden_home_paths(home: &Path, windows_home: bool) -> Vec<PathBuf> {
    let extra: &[&str] = if windows_home {
        READ_HIDDEN_WINDOWS_HOME_EXTRA
    } else {
        &[]
    };
    READ_HIDDEN_HOME_RELATIVE
        .iter()
        .chain(extra)
        .map(|relative| {
            let mut path = home.to_path_buf();
            // Component by component, so that it is spelled with this
            // platform's separators.
            path.extend(relative.split('/'));
            path
        })
        .collect()
}

/// `paths` without every entry that is, or holds, one of `reachable`: the last
/// check on entries already split around the roots (see [`around_reachable`])
/// once each is resolved to where it really lies.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn without_reachable(paths: Vec<PathBuf>, reachable: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| !reachable.iter().any(|kept| kept.starts_with(path)))
        .collect()
}

/// `paths` with every entry that holds one of `reachable` — a worktree root
/// kept under `~/.config/…` — replaced by what lies beside the way down to it:
/// the root stays reachable and the rest of the entry stays hidden (a project
/// at `~/.config/gcloud/x` leaves `~/.config/gcloud/credentials.db` hidden). An
/// entry that *is* one of `reachable` is left out whole; one merely inside a
/// root stays hidden, so a worktree that is the whole home directory exposes no
/// key in it. Lists the directories that hold a root, and leaves out one that
/// cannot be listed rather than hide the root with it.
pub(crate) fn around_reachable(paths: Vec<PathBuf>, reachable: &[PathBuf]) -> Vec<PathBuf> {
    let mut hidden = Vec::new();
    for path in paths {
        split_around(&path, reachable, &mut hidden);
    }
    hidden
}

fn split_around(entry: &Path, reachable: &[PathBuf], hidden: &mut Vec<PathBuf>) {
    if !reachable.iter().any(|kept| kept.starts_with(entry)) {
        push_unique(hidden, entry.to_path_buf());
        return;
    }
    if reachable.iter().any(|kept| kept.as_path() == entry) {
        return;
    }
    let Ok(children) = std::fs::read_dir(entry) else {
        return;
    };
    for child in children.filter_map(std::result::Result::ok) {
        // Covering a symlink would cover whatever it points to, which need not
        // be this entry's at all.
        if child.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        split_around(&child.path(), reachable, hidden);
    }
}

/// ADR-F49 decision 11: the locations whose contents no sandboxed command may
/// read, for an account whose home directory is `home` and a command that must
/// keep reaching `worktree_roots`: [`READ_HIDDEN_HOME_RELATIVE`] (on Windows with
/// all of `AppData`), plus this fork's own directories wherever the environment
/// moved them. An entry that holds one of `worktree_roots` — a project kept
/// under `~/.config/…` — is split around it (see [`around_reachable`]), so the
/// project stays usable and the rest of the entry stays hidden. Some of them
/// may not exist.
///
/// Every backend hides these: the Linux helper and Bubblewrap mount empty
/// content over them, Seatbelt denies reading them. (An SSH host that refuses
/// those mounts runs the command without them and says so.) Exposed for code
/// outside the sandbox that has to enforce the same rule.
pub fn builtin_read_hidden_paths(home: &Path, worktree_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = read_hidden_home_paths(home, cfg!(target_os = "windows"));
    for path in resolve_home_relative(home, LOCAL_READ_HIDDEN_EXTRA) {
        push_unique(&mut paths, path);
    }
    for directory in app_state_directories_from_environment() {
        push_unique(&mut paths, directory);
    }
    around_reachable(paths, worktree_roots)
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

/// ADR-F49: where the IDE bridge keeps each session's rendezvous socket,
/// relative to the distro account's home. It has to be inside
/// [`WSL_SANDBOX_HELPER_HOME_RELATIVE`], which the helper protects in WSL
/// mode: the helper connects to that socket from outside the sandbox, and
/// refuses a rendezvous the sandboxed command could replace.
#[cfg(target_os = "windows")]
pub(crate) const IDE_RENDEZVOUS_HOME_RELATIVE: &str = ".cache/zed-jp-enhance/ide";

/// ADR-F49: where agent binaries staged for the sandbox live, one directory
/// per SHA-256, relative to the distro account's home. Inside
/// [`WSL_SANDBOX_HELPER_HOME_RELATIVE`], so read-only from inside the sandbox.
#[cfg(target_os = "windows")]
pub(crate) const AGENT_BINARIES_HOME_RELATIVE: &str = ".cache/zed-jp-enhance/agents";
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

    /// ADR-F49 decision 11: the read-hidden list resolves against the home, an
    /// entry holding a project the command works in stays reachable, and one
    /// merely inside a worktree — the home itself opened as a project — does
    /// not.
    #[test]
    fn read_hidden_entries_leave_a_project_under_them_reachable() {
        let home = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        };
        let project = home.join(".config").join("gcloud").join("project");
        let paths = builtin_read_hidden_paths(&home, std::slice::from_ref(&project));
        for expected in [
            home.join(".ssh"),
            home.join(".npmrc"),
            home.join(".config").join("gh"),
            home.join(".cargo").join("credentials.toml"),
            home.join(".local").join("share").join("keyrings"),
            home.join(".claude"),
            home.join(".claude.json"),
            home.join(".zed_server"),
        ] {
            assert!(paths.contains(&expected), "{}", expected.display());
        }
        assert!(
            !paths.contains(&home.join(".config").join("gcloud")),
            "the entry holding the project stays reachable"
        );
        assert_eq!(
            cfg!(target_os = "windows"),
            paths.contains(&home.join("AppData")),
            "all of AppData is hidden on Windows homes, and only there"
        );

        let home_as_project = builtin_read_hidden_paths(&home, std::slice::from_ref(&home));
        assert!(
            home_as_project.contains(&home.join(".ssh")),
            "an entry inside a worktree stays hidden"
        );
    }

    /// A root the command works in, kept inside a hidden entry, stays
    /// reachable while everything else in the entry stays hidden.
    #[test]
    fn a_root_inside_a_hidden_entry_keeps_the_rest_of_the_entry_hidden() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let entry = temporary.path().join("gcloud");
        let project = entry.join("configurations").join("project");
        std::fs::create_dir_all(&project).expect("create");
        std::fs::write(entry.join("credentials.db"), "secret").expect("write");
        std::fs::write(entry.join("configurations").join("config_default"), "x").expect("write");

        let hidden = around_reachable(vec![entry.clone()], std::slice::from_ref(&project));
        assert!(hidden.contains(&entry.join("credentials.db")), "{hidden:?}");
        assert!(
            hidden.contains(&entry.join("configurations").join("config_default")),
            "{hidden:?}"
        );
        assert!(
            !hidden.iter().any(|path| project.starts_with(path)),
            "nothing on the way down to the root is hidden: {hidden:?}"
        );
        assert!(
            around_reachable(vec![project.clone()], std::slice::from_ref(&project)).is_empty(),
            "an entry that is the root is left out"
        );
    }

    /// A symlink beside the way down to a root is not covered: covering it
    /// would cover whatever it points to.
    #[cfg(unix)]
    #[test]
    fn a_symlink_beside_the_way_down_is_not_covered() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let entry = temporary.path().join("gcloud");
        let project = entry.join("project");
        std::fs::create_dir_all(&project).expect("create");
        let elsewhere = temporary.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("create");
        std::os::unix::fs::symlink(&elsewhere, entry.join("link")).expect("symlink");
        std::fs::write(entry.join("credentials.db"), "secret").expect("write");

        let hidden = around_reachable(vec![entry.clone()], std::slice::from_ref(&project));
        assert!(hidden.contains(&entry.join("credentials.db")), "{hidden:?}");
        assert!(!hidden.contains(&entry.join("link")), "{hidden:?}");
    }

    /// The Windows-home variant the WSL helper applies to every Windows user.
    #[test]
    fn windows_homes_hide_all_of_app_data_and_underscore_netrc() {
        let home = PathBuf::from("/mnt/c/Users/u");
        let windows = read_hidden_home_paths(&home, true);
        let unix = read_hidden_home_paths(&home, false);
        for extra in ["AppData", "_netrc"] {
            assert!(windows.contains(&home.join(extra)), "{extra}");
            assert!(!unix.contains(&home.join(extra)), "{extra}");
        }
        assert_eq!(
            windows.len(),
            READ_HIDDEN_HOME_RELATIVE.len() + READ_HIDDEN_WINDOWS_HOME_EXTRA.len()
        );
    }

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

    /// The IDE rendezvous and the staged agent binaries sit inside the
    /// directory the helper protects in WSL mode.
    #[cfg(target_os = "windows")]
    #[test]
    fn adr_f49_directories_are_inside_the_protected_install_root() {
        for relative in [IDE_RENDEZVOUS_HOME_RELATIVE, AGENT_BINARIES_HOME_RELATIVE] {
            assert!(
                relative.starts_with(&format!("{WSL_SANDBOX_HELPER_HOME_RELATIVE}/")),
                "{relative}"
            );
        }
    }

    /// Each worktree root gets every agent configuration, spelled with this
    /// platform's separators, and a root named twice gets them once.
    #[test]
    fn agent_configurations_are_resolved_in_every_worktree_root() {
        for relative in WORKTREE_PROTECTED_RELATIVE {
            assert!(
                !relative.starts_with('/')
                    && relative
                        .split('/')
                        .all(|component| !matches!(component, "" | "." | "..")),
                "{relative} must be a plain relative path"
            );
        }
        let (first, second) = if cfg!(target_os = "windows") {
            (PathBuf::from(r"C:\work\one"), PathBuf::from(r"D:\two"))
        } else {
            (PathBuf::from("/work/one"), PathBuf::from("/two"))
        };
        let paths = worktree_protected_paths(&[first.clone(), second.clone(), first.clone()]);
        assert_eq!(paths.len(), 2 * WORKTREE_PROTECTED_RELATIVE.len());
        for root in [&first, &second] {
            for expected in [
                root.join(".claude"),
                root.join(".mcp.json"),
                root.join(".gemini"),
                root.join(".codex"),
            ] {
                assert!(paths.contains(&expected), "{}", expected.display());
            }
        }
    }

    /// The public list is the local home-relative set — git's global
    /// configuration and, on Windows, the `PATH` shims among it — plus the
    /// worktree entries.
    #[test]
    fn the_public_list_holds_the_home_and_worktree_entries() {
        let (home, worktree) = if cfg!(target_os = "windows") {
            (PathBuf::from(r"C:\Users\u"), PathBuf::from(r"C:\work"))
        } else {
            (PathBuf::from("/home/u"), PathBuf::from("/work"))
        };
        let paths = builtin_protected_paths(&home, std::slice::from_ref(&worktree));
        assert!(paths.contains(&home.join(".gitconfig")));
        assert!(paths.contains(&home.join(".config").join("git")));
        for agent_configuration in [".claude.json", ".claude", ".gemini", ".codex"] {
            assert!(
                paths.contains(&home.join(agent_configuration)),
                "{agent_configuration}"
            );
        }
        assert!(paths.contains(&worktree.join(".mcp.json")));
        if cfg!(target_os = "windows") {
            assert!(paths.contains(&home.join("AppData").join("Roaming").join("npm")));
            assert!(
                paths.contains(
                    &home
                        .join("AppData")
                        .join("Local")
                        .join("Microsoft")
                        .join("WindowsApps")
                )
            );
        }
        for path in local_builtin_protected_paths(&home, &[]) {
            assert!(paths.contains(&path), "{}", path.display());
        }
    }
}
