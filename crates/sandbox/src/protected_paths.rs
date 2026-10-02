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
///
/// It is also what the client refuses for an external agent's write to a
/// remote host, whose home it can't see, and what a verified write refuses
/// below the account's home. `fs::verified_write` keeps its own copy, so that
/// `fs` doesn't depend on this crate; a test in `acp_thread` keeps the two
/// the same.
pub const UNIX_PROTECTED_HOME_RELATIVE: &[&str] = &[
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
    // ADR-F49 decision 8 (2026-09-15): the rest of what decision 11 hides from
    // reads. Hiding a secret's contents and leaving it writable is not a
    // half-measure but a different hole — a command that can rewrite
    // `~/.npmrc` or `~/.cargo/credentials.toml` points the user's own next
    // `npm install` or `cargo publish` at a registry of its choosing, and one
    // that can write `~/.config/gcloud` does the same to the cloud CLI the
    // user runs outside the sandbox.
    //
    // It only ever comes up where the worktree *is* or *holds* the home, but
    // that is a configuration this fork's own saved connections open. On the
    // backends that hide reads with a read-only cover the cover happened to
    // stop the write as well; on the Landlock backend (decision 28, a host
    // that refuses the mounts) nothing did, which is what this closes. Every
    // backend now takes the same list from here.
    ".npmrc",
    ".yarnrc.yml",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gcloud",
    ".terraform.d",
    ".local/share/keyrings",
    ".password-store",
    // Browser profiles: cookies and saved passwords to read, and an extension
    // or a preferences file to write into the browser the user runs.
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
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
    // executes, and the state directory beside it. Its *configuration*
    // directory is deliberately not here: this list is also what refuses an
    // external agent's `fs/write_text_file` outright, and a write to Zed's
    // own `settings.json` is one the user is asked about instead
    // (`agent_write_guard`). Against a sandboxed command it is protected all
    // the same, by the local list below and by the helper, which keeps out of
    // writes everything it hides from reads.
    ".zed_server",
    // The data directory under the name this build uses and under every
    // earlier one: a build from before the rename can be the next to run.
    branding::concat_const!(
        ".local/share/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/share/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
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
    // ADR-F49 decision 8 (2026-09-15): the rest of what decision 11 hides from
    // reads, in the spellings the Windows tools use — rewriting one of these
    // points the user's own next `npm install` / `cargo publish` / `gcloud`
    // at whatever the command chose. `AppData` is deliberately not here even
    // though the hiding covers it whole: protecting it flat would refuse any
    // worktree below it, and on this platform the hiding is always a
    // read-only cover inside WSL (the Landlock path decision 28 added is
    // Linux-only and never plans on a WSL kernel), so nothing can write
    // through it.
    r".npmrc",
    r".yarnrc.yml",
    r".pypirc",
    r".cargo\credentials",
    r".cargo\credentials.toml",
    r".config\gcloud",
    r"AppData\Roaming\gcloud",
    r".terraform.d",
    r".password-store",
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
    // thread database, and the logs. Under every name this fork has used: the
    // next build to run can be one from before the rename.
    branding::concat_const!(
        r"AppData\Roaming\",
        branding::FORGEKAIROS.app_directory_name
    ),
    branding::concat_const!(r"AppData\Local\", branding::FORGEKAIROS.app_directory_name),
    branding::concat_const!(
        r"AppData\Roaming\",
        branding::ZED_JP_ENHANCE.app_directory_name
    ),
    branding::concat_const!(
        r"AppData\Local\",
        branding::ZED_JP_ENHANCE.app_directory_name
    ),
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
/// ADR-F49 decision 15: [`SANDBOXED_AGENT_CONFIG_HOME_RELATIVE`] is in both
/// lists because a sandboxed agent's own configuration — the hooks it runs and
/// the `apiKeyHelper` it calls — is read by the *next* session for that
/// project: a command that can write another project's configuration there has
/// injected code into that session. The session that owns one of them writes it
/// through its write grant, and the directory is then covered around that grant
/// rather than protected flat (see [`enclosures_to_cover`] and
/// [`builtin_protected_paths`]).
#[cfg(target_os = "linux")]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[
    branding::concat_const!(
        ".config/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".config/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    ".config/autostart",
    ".config/systemd/user",
    SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
    LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
];

/// macOS also keeps this fork's data, and the browsers' and the system's
/// secret stores, outside the XDG-style locations of the shared list. Decision
/// 8 keeps this in step with [`LOCAL_READ_HIDDEN_EXTRA`]: what is hidden from
/// reads is not writable either.
#[cfg(target_os = "macos")]
const LOCAL_EXTRA_PROTECTED_HOME_RELATIVE: &[&str] = &[
    branding::concat_const!(
        "Library/Application Support/",
        branding::FORGEKAIROS.app_directory_name
    ),
    branding::concat_const!(
        ".config/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(".local/state/", branding::FORGEKAIROS.app_directory_name),
    branding::concat_const!(
        "Library/Application Support/",
        branding::ZED_JP_ENHANCE.app_directory_name
    ),
    branding::concat_const!(
        ".config/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    branding::concat_const!(".local/state/", branding::ZED_JP_ENHANCE.app_directory_name),
    "Library/LaunchAgents",
    SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
    LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
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
    branding::CURRENT.app_directory_name_lowercase
} else {
    branding::CURRENT.app_directory_name
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
    // Unique: the two lists are kept apart by who needs each entry rather than
    // by what is in them, so one that belongs on both would otherwise be
    // pinned twice.
    for path in resolve_home_relative(home, LOCAL_EXTRA_PROTECTED_HOME_RELATIVE) {
        push_unique(&mut paths, path);
    }
    for directory in app_state_directories {
        push_unique(&mut paths, directory.clone());
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
/// ([`WORKTREE_PROTECTED_RELATIVE`]), except in a sandboxed agent's own
/// configuration directory (see [`is_sandboxed_agent_configuration`]). Some of
/// them may not exist.
///
/// For code outside the sandbox that has to enforce the same rule, such as the
/// guard on an external agent's file writes, so that it uses this list rather
/// than a copy of it. `Sandbox::new` takes `home` from `HOME` (on Windows
/// `USERPROFILE`), falling back to the account database, and the worktree
/// roots from the directories it grants write access to.
pub fn builtin_protected_paths(home: &Path, worktree_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = local_builtin_protected_paths(home, &protected_directories_from_process());
    // A directory covered around a root inside it is protected by the cover —
    // an empty read-only one, with only that root bound back into it — and
    // protecting it flat as well would take the root away again. Where the
    // cover does not apply (it is not the real directory it is named as, or
    // nothing is written inside it) the entry stays, and a write grant inside
    // it is then refused like any other overlap.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let roots: Vec<&Path> = worktree_roots.iter().map(PathBuf::as_path).collect();
        let covered = enclosures_to_cover(&[sandboxed_agent_configuration_directory(home)], &roots);
        paths.retain(|path| {
            !covered.iter().any(|enclosure| {
                path == enclosure
                    || std::fs::canonicalize(path).is_ok_and(|resolved| resolved == *enclosure)
            })
        });
    }
    for path in worktree_protected_paths(home, worktree_roots) {
        push_unique(&mut paths, path);
    }
    paths
}

/// How deep in the sandboxed agents' projects directory one agent's own
/// configuration directory for one project lies: `<project key>/<agent
/// directory>` (ADR-F49 decision 15 — Claude's is `config`, Codex's `codex`,
/// a custom agent's `agent-<id>`).
const AGENT_CONFIGURATION_DEPTH: usize = 2;

/// ADR-F49 decision 15: whether `root`, a directory a command may write, is one
/// agent's own configuration directory for one project — exactly
/// `<projects>/<project key>/<agent directory>` under `home`, no deeper and no
/// shallower. No agent on the host reads one as configuration; only the
/// sandboxed agent pointed at it does. So the names a host agent acts on in a
/// worktree are not protected in it, and the agent can create what it keeps
/// right at its top (its credentials, at the first login).
///
/// Decided on where `root` really lies, never from a flag or the environment,
/// and `root` must already *be* where it lies: a path that resolves elsewhere
/// (a symlink planted in the projects directory leading into a worktree) is a
/// worktree like any other, which is the same rule the remote helper applies to
/// a write grant it must trust (`CanonicalPathBuf::from_canonical`). A root
/// that does not resolve is taken for a worktree too.
pub(crate) fn is_sandboxed_agent_configuration(root: &Path, home: &Path) -> bool {
    let mut projects = home.to_path_buf();
    // Component by component, so that it is spelled with this platform's
    // separators.
    projects.extend(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE.split('/'));
    let projects = std::fs::canonicalize(&projects).unwrap_or(projects);
    if !std::fs::canonicalize(root).is_ok_and(|resolved| resolved == root) {
        return false;
    }
    root.strip_prefix(&projects).is_ok_and(|relative| {
        let mut components = relative.components();
        components.all(|component| matches!(component, std::path::Component::Normal(_)))
            && relative.components().count() == AGENT_CONFIGURATION_DEPTH
    })
}

/// Paths, relative to a worktree root, that a program on the *host* reads and
/// acts on: Claude Code's project directory and MCP servers, and Gemini CLI's
/// and Codex's project directories. The whole `.claude/` is protected, not only
/// `settings.json`: the host Claude Code also runs the hook scripts under
/// `.claude/hooks/` and the commands and skills beside them. A sandboxed
/// command writing any of these would configure — through hooks and MCP
/// servers, run — code outside the sandbox the next time a host agent opens the
/// project. So they are protected in every worktree root a command may write,
/// as its `.git` is (ADR-F49). The guard on an external agent's file writes
/// asks the user before it writes one of these.
pub const WORKTREE_PROTECTED_RELATIVE: &[&str] = &[".claude", ".mcp.json", ".gemini", ".codex"];

/// [`WORKTREE_PROTECTED_RELATIVE`] resolved against each of `worktree_roots`
/// but the sandboxed agents' own configuration directories under `home` (see
/// [`is_sandboxed_agent_configuration`]).
pub(crate) fn worktree_protected_paths(home: &Path, worktree_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in worktree_roots {
        if is_sandboxed_agent_configuration(root, home) {
            continue;
        }
        for path in worktree_entries(root) {
            push_unique(&mut paths, path);
        }
    }
    paths
}

/// [`WORKTREE_PROTECTED_RELATIVE`] resolved against one root, for a caller that
/// decides for itself whether the root is a worktree (the remote helper, which
/// judges a root only after pinning it).
pub(crate) fn worktree_entries(root: &Path) -> Vec<PathBuf> {
    WORKTREE_PROTECTED_RELATIVE
        .iter()
        .map(|relative| {
            let mut path = root.to_path_buf();
            // Component by component, so that it is spelled with this
            // platform's separators.
            path.extend(relative.split('/'));
            path
        })
        .collect()
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

/// [`protected_directories_from_environment`] plus the directories this process
/// really uses, which it registered at startup
/// ([`crate::register_application_directories`]): a `--user-data-dir` puts the
/// settings, the thread database and the logs where neither the home-relative
/// defaults nor the environment name them, and a sandboxed command must not be
/// able to write them.
pub(crate) fn protected_directories_from_process() -> Vec<PathBuf> {
    let mut directories = protected_directories_from_environment();
    for directory in crate::registered_application_directories() {
        push_unique(&mut directories, directory);
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
    SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
    LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
    branding::concat_const!(
        ".config/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/share/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::FORGEKAIROS.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".config/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/share/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    branding::concat_const!(
        ".local/state/",
        branding::ZED_JP_ENHANCE.app_directory_name_lowercase
    ),
    ".zed_server",
];

/// ADR-F49: where the Claude Code sessions Zed runs in the WSL sandbox keep
/// their configuration, one directory per project, relative to the distro
/// account's home. It holds their login tokens and the hooks they run, so no
/// other sandboxed command may read it (a session reaches its own directory
/// through its write grant, see [`around_reachable`]), and in WSL none may
/// write it either.
pub const SANDBOXED_AGENT_CONFIG_HOME_RELATIVE: &str =
    branding::CURRENT.sandboxed_agent_config_home_relative;

/// [`SANDBOXED_AGENT_CONFIG_HOME_RELATIVE`] as builds from before the rename
/// named it. It holds the same login tokens and hooks until they are moved to
/// the new name, and a build from before the rename keeps using it, so it is
/// hidden and protected the same way, as a whole: no session of this build is
/// granted anything inside it.
pub const LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE: &str =
    branding::ZED_JP_ENHANCE.sandboxed_agent_config_home_relative;

/// ADR-F49: the directory below [`SANDBOXED_AGENT_CONFIG_HOME_RELATIVE`] that
/// holds each project's configuration directory, relative to the distro
/// account's home. A session is granted its own project's directory in it; the
/// WSL helper covers the rest of it whole, so that a project added while the
/// session runs is not visible to it either.
pub const SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE: &str =
    branding::concat_const!(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE, "/projects");

/// What [`migrate_legacy_sandboxed_agent_configuration`] did.
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
pub enum LegacyAgentConfigurationMigration {
    /// The directory under the earlier name now has the current one.
    Moved,
    /// There was no directory under the earlier name.
    NothingToMove,
    /// The current one exists already (or the two names are the same): the
    /// earlier one, if any, is left where it is.
    AlreadyThere,
    /// The earlier one is not a directory of this account's own, reached
    /// without a symbolic link, and is left where it is.
    NotOwnDirectory,
}

/// Gives the sandboxed agents' configuration directory under `home` its
/// current name ([`SANDBOXED_AGENT_CONFIG_HOME_RELATIVE`]) when only the
/// directory under the name builds from before the rename used is there, so
/// that the agents stay signed in.
///
/// Only a real directory this account owns is moved, never one that is or is
/// reached through a symbolic link: the move must not take something from
/// elsewhere, or someone else's, into the place the sandbox treats as this
/// account's agents' own. On Linux the rename itself refuses to replace a
/// current one that appeared since it was looked for.
#[cfg(any(target_os = "linux", test))]
pub fn migrate_legacy_sandboxed_agent_configuration_in(
    home: &Path,
) -> std::io::Result<LegacyAgentConfigurationMigration> {
    if LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE == SANDBOXED_AGENT_CONFIG_HOME_RELATIVE {
        return Ok(LegacyAgentConfigurationMigration::AlreadyThere);
    }
    let current = home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
    let legacy = home.join(LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
    match std::fs::symlink_metadata(&current) {
        Ok(_) => return Ok(LegacyAgentConfigurationMigration::AlreadyThere),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let metadata = match std::fs::symlink_metadata(&legacy) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LegacyAgentConfigurationMigration::NothingToMove);
        }
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || !is_owned_by_this_account(&metadata) {
        return Ok(LegacyAgentConfigurationMigration::NotOwnDirectory);
    }
    // The home itself may be reached through a link (`/home` on a separate
    // volume); what must not be a link is the directory being moved, which
    // `symlink_metadata` above has looked at without following it.
    rename_without_replacing(&legacy, &current)?;
    Ok(LegacyAgentConfigurationMigration::Moved)
}

#[cfg(target_os = "linux")]
fn is_owned_by_this_account(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    metadata.uid() == unsafe { libc::geteuid() }
}

/// Elsewhere (the tests on Windows and macOS) everything under a temporary
/// home is this account's.
#[cfg(all(not(target_os = "linux"), test))]
fn is_owned_by_this_account(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both are NUL-terminated strings that outlive the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Without `renameat2` (the tests elsewhere) the destination was checked to
/// be missing just before, which is as close as a plain rename gets.
#[cfg(all(not(target_os = "linux"), test))]
fn rename_without_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// [`migrate_legacy_sandboxed_agent_configuration_in`] for this account's home,
/// as the account database names it. For the processes on the Linux side that
/// run before any sandboxed agent session: the remote server when it starts on
/// an SSH host, and the WSL helper's probe.
#[cfg(target_os = "linux")]
pub fn migrate_legacy_sandboxed_agent_configuration()
-> anyhow::Result<LegacyAgentConfigurationMigration> {
    use anyhow::Context as _;
    let home = home_dir_from_account().context("cannot determine the account's home directory")?;
    migrate_legacy_sandboxed_agent_configuration_in(&home).with_context(|| {
        format!(
            "moving {} to {} in {}",
            LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
            SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
            home.display()
        )
    })
}

/// What a Windows home hides on top of [`READ_HIDDEN_HOME_RELATIVE`]: `_netrc`,
/// the spelling curl looks for there, and all of `AppData`, where browser
/// profiles, application tokens and this fork's own data live.
pub(crate) const READ_HIDDEN_WINDOWS_HOME_EXTRA: &[&str] = &["_netrc", "AppData"];

/// macOS keeps this fork's data, and the browsers' and the system's secret
/// stores, outside the XDG-style locations above.
#[cfg(target_os = "macos")]
const LOCAL_READ_HIDDEN_EXTRA: &[&str] = &[
    branding::concat_const!(
        "Library/Application Support/",
        branding::FORGEKAIROS.app_directory_name
    ),
    branding::concat_const!(".local/state/", branding::FORGEKAIROS.app_directory_name),
    branding::concat_const!(
        "Library/Application Support/",
        branding::ZED_JP_ENHANCE.app_directory_name
    ),
    branding::concat_const!(".local/state/", branding::ZED_JP_ENHANCE.app_directory_name),
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

/// ADR-F49 decision 11 on a host that refuses the covers (Ubuntu 24.04's
/// default `apparmor_restrict_unprivileged_userns=1` leaves an unprivileged
/// user namespace unable to mount): the locations a Landlock ruleset grants
/// reads on, so that nothing at or below `denied` can be read, listed or
/// executed while everything else still can.
///
/// Landlock only ever *adds* access, and a right on a directory reaches
/// everything below it, so an entry is hidden by giving a rule neither to it
/// nor to any of its ancestors: `root` is granted whole unless something denied
/// lies below it, and otherwise its entries are granted one by one, descending
/// into each that leads further down to a denied entry. This is the read-side
/// mirror of the write carve and carries the mirror image of its cost — a
/// directory on the way down to a denied entry (the home itself, `~/.config`)
/// is never granted, so *listing* it is refused, and an entry created in it
/// after the command started cannot be read either.
///
/// `list` gives a directory's entries, each where it really lies (symlinks
/// followed), and `None` for one that cannot be listed — which grants nothing
/// below it, denying reads there rather than handing out the entry it was
/// carved around. An entry resolving outside the directory that holds it is
/// left to wherever it really lies: granting `link -> /` through it would hand
/// out every denied entry with it.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn carve_read_grants(
    root: &Path,
    denied: &[PathBuf],
    list: &mut dyn FnMut(&Path) -> Option<Vec<PathBuf>>,
) -> Vec<PathBuf> {
    let mut grants = Vec::new();
    carve_read_subtree(root, denied, list, &mut grants);
    grants
}

#[cfg(any(target_os = "linux", test))]
fn carve_read_subtree(
    directory: &Path,
    denied: &[PathBuf],
    list: &mut dyn FnMut(&Path) -> Option<Vec<PathBuf>>,
    grants: &mut Vec<PathBuf>,
) {
    if denied.iter().any(|entry| directory.starts_with(entry)) {
        return;
    }
    if !denied.iter().any(|entry| entry.starts_with(directory)) {
        push_unique(grants, directory.to_path_buf());
        return;
    }
    let Some(children) = list(directory) else {
        return;
    };
    for child in children {
        if !child.starts_with(directory) || child == directory {
            continue;
        }
        carve_read_subtree(&child, denied, list, grants);
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
    around_reachable(read_hidden_entries(home), worktree_roots)
}

/// The locations whose contents no sandboxed command may read, before they are
/// split around anything: [`READ_HIDDEN_HOME_RELATIVE`] under `home` (on
/// Windows with all of `AppData`), this platform's extras, and this fork's own
/// directories wherever the environment moved them or this process registered
/// them ([`crate::register_application_directories`] — otherwise a
/// `--user-data-dir` leaves the thread database and the logs readable).
pub(crate) fn read_hidden_entries(home: &Path) -> Vec<PathBuf> {
    let mut paths = read_hidden_home_paths(home, cfg!(target_os = "windows"));
    for path in resolve_home_relative(home, LOCAL_READ_HIDDEN_EXTRA) {
        push_unique(&mut paths, path);
    }
    for directory in app_state_directories_from_environment()
        .into_iter()
        .chain(crate::registered_application_directories())
    {
        push_unique(&mut paths, directory);
    }
    paths
}

/// ADR-F49 decision 14: this fork's data directory (`paths::data_dir()` where
/// no environment variable moved it), relative to a home, on the platforms
/// whose sandbox decides what a command may be shown inside it.
#[cfg(target_os = "linux")]
const DATA_DIRECTORY_HOME_RELATIVE: &str = branding::concat_const!(
    ".local/share/",
    branding::CURRENT.app_directory_name_lowercase
);
#[cfg(target_os = "macos")]
const DATA_DIRECTORY_HOME_RELATIVE: &str = branding::concat_const!(
    "Library/Application Support/",
    branding::CURRENT.app_directory_name
);

/// ADR-F49 decision 14: what inside this fork's data directory a command may
/// be shown although the read hiding covers the directory: the agents Zed
/// installed (`external_agents`: the registry's binaries and npx packages) and
/// the Node.js Zed runs them with (`node`). Nothing else in it — the thread
/// database, the logs, the remote server's state — ever is.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) const AGENT_RUNTIME_DATA_RELATIVE: &[&str] = &["external_agents", "node"];

/// Why a command may not be shown a location (ADR-F49 decision 14).
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(crate) enum ReadableRefusal {
    /// It could not be resolved: it does not exist, or cannot be reached.
    Unresolvable(std::io::Error),
    /// Where it really lies is outside every agent installation and Zed's
    /// Node.js.
    OutsideAgentRuntime {
        canonical: PathBuf,
        data_directory: PathBuf,
    },
    /// It is, holds or lies inside another entry whose contents stay hidden.
    TouchesHiddenEntry { canonical: PathBuf, entry: PathBuf },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl std::fmt::Display for ReadableRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadableRefusal::Unresolvable(error) => {
                write!(formatter, "cannot be resolved: {error}")
            }
            ReadableRefusal::OutsideAgentRuntime {
                canonical,
                data_directory,
            } => {
                write!(
                    formatter,
                    "resolves to {}, which is inside neither an agent installation nor \
                     this {}'s Node.js (",
                    canonical.display(),
                    branding::CURRENT.product_name
                )?;
                for (index, relative) in AGENT_RUNTIME_DATA_RELATIVE.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{}", data_directory.join(relative).display())?;
                }
                write!(formatter, ")")
            }
            ReadableRefusal::TouchesHiddenEntry { canonical, entry } => write!(
                formatter,
                "resolves to {}, which overlaps {}, whose contents stay hidden",
                canonical.display(),
                entry.display()
            ),
        }
    }
}

/// ADR-F49 decision 14: where this fork keeps the agents it installed and the
/// Node.js it runs them with — the data directory this process registered
/// ([`crate::register_application_directories`]), or the default place under
/// `home` where nothing was registered: an embedder that never registers one.
///
/// The **in-process** sandbox (local bwrap, Seatbelt) uses this: the editor has
/// registered its real data directory, so a `--user-data-dir` or moved
/// `XDG_DATA_HOME` is already accounted for without this crate reading the
/// environment (decision 27). The SSH-remote helper runs in a separate process
/// where nothing is registered and must instead learn the location from the
/// environment — see [`remote_agent_runtime_data_directory`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn agent_runtime_data_directory(home: &Path) -> PathBuf {
    crate::registered_data_directory().unwrap_or_else(|| home.join(DATA_DIRECTORY_HOME_RELATIVE))
}

/// ADR-F49 decisions 14 and 27: the data directory the **SSH-remote helper**
/// looks in for the agent installations and Node.js a command may be shown
/// read-only, and whose cover keeps the rest of it hidden.
///
/// The helper is a separate process from the remote server that spawned it (it
/// re-executes the server binary), so nothing is registered in it
/// ([`crate::register_application_directories`] runs only in the editor) and
/// [`agent_runtime_data_directory`] would resolve to the home-relative default
/// alone. It must instead learn the location the same way the read hiding does:
/// from the environment (`XDG_DATA_HOME`, the base `paths::data_dir()` follows
/// on the remote host), falling back to the home-relative default where the
/// variable is unset — exactly the data directory
/// [`app_state_directories_from_environment`] already covers.
///
/// Trusting the environment for the *location* here adds no attack surface. The
/// read hiding already hides this same env-named directory, so pointing
/// `readable` at it only lets `external_agents`/`node` back out of a directory
/// that is otherwise hidden — never a new one, and only when it actually
/// exists (see [`resolve_readable_path`]). The list of what stays hidden is
/// unchanged (decision 11): the environment moves the folder, it does not
/// choose the entries.
#[cfg(target_os = "linux")]
pub(crate) fn remote_agent_runtime_data_directory(home: &Path) -> PathBuf {
    app_state_directory_from_environment("XDG_DATA_HOME")
        .unwrap_or_else(|| home.join(DATA_DIRECTORY_HOME_RELATIVE))
}

/// ADR-F49 decision 14: where `requested` really lies, when a command may be
/// shown it although the read hiding covers it — inside one of
/// [`AGENT_RUNTIME_DATA_RELATIVE`] in `data_directory`, and neither holding nor
/// inside any other entry that stays hidden.
///
/// Decided on the resolved path, so a symlink leading anywhere else is refused.
/// The allowed directories themselves are named *below* the resolved data
/// directory and are deliberately not resolved: an `external_agents` or `node`
/// that is a link to the data directory — or to anything holding it — would
/// otherwise make all of it showable, credential stores kept inside it
/// included. Which data directory that is comes from the application, never
/// from the environment: a variable a project can set must not be a way to
/// un-hide a credential store. The environment only adds entries to stay clear
/// of.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn resolve_readable_path(
    requested: &Path,
    data_directory: &Path,
    home: &Path,
) -> Result<PathBuf, ReadableRefusal> {
    let resolved = |path: PathBuf| std::fs::canonicalize(&path).unwrap_or(path);
    let canonical = std::fs::canonicalize(requested).map_err(ReadableRefusal::Unresolvable)?;
    let data_directory = resolved(data_directory.to_path_buf());
    let inside_runtime = AGENT_RUNTIME_DATA_RELATIVE
        .iter()
        .any(|relative| canonical.starts_with(data_directory.join(relative)));
    if !inside_runtime {
        return Err(ReadableRefusal::OutsideAgentRuntime {
            canonical,
            data_directory,
        });
    }
    for entry in read_hidden_entries(home) {
        let entry = resolved(entry);
        if entry == data_directory {
            continue;
        }
        if canonical.starts_with(&entry) || entry.starts_with(&canonical) {
            return Err(ReadableRefusal::TouchesHiddenEntry { canonical, entry });
        }
    }
    Ok(canonical)
}

/// ADR-F49 decisions 14 and 15: the directories a command is shown nothing of
/// but what it is granted inside them — covered whole with empty content and
/// only those grants bound back — rather than by hiding their entries one by
/// one, which could only hide the entries there when the command started:
///
/// * the sandboxed agents' configuration directory under `home`, holding one
///   directory per project: a session sees its own project's and no other's,
///   however many are added while it runs, and cannot write anything else in
///   it (decision 15, and the write protection of ADR-F49 decision 8 for a
///   session that writes its own);
/// * `data_directory`, where the agent installations and Node.js a command may
///   be shown lie among the rest of this fork's data (decision 14): the thread
///   database, the settings and the logs stay hidden, and so does anything
///   added to it while the command runs.
///
/// Where they really are, as the roots and the read-hidden targets are: under
/// a home reached through a symlink they are covered where they really lie.
/// A directory only gets a cover when the command is granted something inside
/// it (see [`enclosures_to_cover`]); otherwise the ordinary hiding applies.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn enclosed_directories(home: &Path) -> Vec<PathBuf> {
    enclosed_directories_in(home, &agent_runtime_data_directory(home))
}

/// As [`enclosed_directories`], for a caller that already knows the data
/// directory rather than deriving it from the registration: the SSH-remote
/// helper, which learns it from the environment
/// ([`remote_agent_runtime_data_directory`]) because nothing is registered in
/// its process. The readable check and this cover then agree on the one data
/// directory, so what a command may be shown is exactly what is bound back.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn enclosed_directories_in(home: &Path, data_directory: &Path) -> Vec<PathBuf> {
    let mut directories = vec![sandboxed_agent_configuration_directory(home)];
    push_unique(
        &mut directories,
        std::fs::canonicalize(data_directory).unwrap_or_else(|_| data_directory.to_path_buf()),
    );
    directories
}

/// The sandboxed agents' configuration directory under `home`, where it really
/// is. It is the one enclosure whose *write* protection comes from its cover
/// instead of the always-protected set (see [`builtin_protected_paths`]): the
/// session that owns one directory in it writes that directory. The data
/// directory stays protected whether or not it is covered — what a command may
/// be shown in it, it may never write.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn sandboxed_agent_configuration_directory(home: &Path) -> PathBuf {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE)
}

/// Those of `enclosures` to cover for a command granted `granted` inside them —
/// the roots it may write and the locations it may read: a real directory where
/// it is named, which holds one of them without being one itself. Anything else
/// is left to the split around the roots (see [`around_reachable`]): a cover
/// with nothing to bind back into it would only take away what the command was
/// never given, and one on a symlink would land where the link points.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn enclosures_to_cover(enclosures: &[PathBuf], granted: &[&Path]) -> Vec<PathBuf> {
    enclosures
        .iter()
        .filter(|enclosure| {
            std::fs::symlink_metadata(enclosure).is_ok_and(|metadata| metadata.is_dir())
                && std::fs::canonicalize(enclosure).is_ok_and(|canonical| canonical == **enclosure)
                && granted.iter().all(|path| *path != enclosure.as_path())
                && granted.iter().any(|path| path.starts_with(enclosure))
        })
        .cloned()
        .collect()
}

/// Where the Windows backend installs its WSL sandbox helper (the bundled Linux
/// `remote_server` and `slirp4netns`), relative to the distro account's home.
///
/// Inside WSL the helper itself protects the distro account's home — the Unix
/// list, exactly as on an SSH host — and in WSL mode this directory as well:
/// overwriting the binary that runs before every sandboxed command would be
/// code execution outside the sandbox (the WSL counterpart of `.zed_server`).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_SANDBOX_HELPER_HOME_RELATIVE: &str =
    branding::CURRENT.remote_cache_home_relative;

/// [`WSL_SANDBOX_HELPER_HOME_RELATIVE`] as builds from before the rename named
/// it. It is the remote server's cache directory too, which the server moves
/// to the current name only when nothing is there under that name yet; a
/// build from before the rename still installs and runs its helper under the
/// earlier name, so it is protected alongside the current one either way.
#[cfg(any(target_os = "linux", all(test, target_os = "windows")))]
pub(crate) const LEGACY_WSL_SANDBOX_HELPER_HOME_RELATIVE: &str =
    branding::ZED_JP_ENHANCE.remote_cache_home_relative;

/// ADR-F49: where the IDE bridge keeps each session's rendezvous socket,
/// relative to the distro account's home. It has to be inside
/// [`WSL_SANDBOX_HELPER_HOME_RELATIVE`], which the helper protects in WSL
/// mode: the helper connects to that socket from outside the sandbox, and
/// refuses a rendezvous the sandboxed command could replace.
#[cfg(target_os = "windows")]
pub(crate) const IDE_RENDEZVOUS_HOME_RELATIVE: &str =
    branding::concat_const!(branding::CURRENT.remote_cache_home_relative, "/ide");

/// ADR-F49: where agent binaries staged for the sandbox live, one directory
/// per SHA-256, relative to the distro account's home. Inside
/// [`WSL_SANDBOX_HELPER_HOME_RELATIVE`], so read-only from inside the sandbox.
#[cfg(target_os = "windows")]
pub(crate) const AGENT_BINARIES_HOME_RELATIVE: &str =
    branding::concat_const!(branding::CURRENT.remote_cache_home_relative, "/agents");
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
    account_from_passwd().map(|account| account.home)
}

/// The account this process acts as, from the account database alone (its home
/// directory and its login name). For the sandbox helper, which builds the
/// command's environment from nothing and cannot trust its own — the client on
/// a remote host does not know where the account's home is, so `HOME` / `USER`
/// / `LOGNAME` are read here rather than passed in.
#[cfg(target_os = "linux")]
pub(crate) struct Account {
    pub home: PathBuf,
    pub name: std::ffi::OsString,
}

#[cfg(target_os = "linux")]
pub(crate) fn account_from_passwd() -> Option<Account> {
    read_passwd()
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

/// See [`home_dir_from_account`]: the home directory alone.
#[cfg(target_os = "linux")]
fn home_dir_from_passwd() -> Option<PathBuf> {
    read_passwd().map(|account| account.home)
}

/// `getpwuid(geteuid())`: the home directory (which must be absolute) and login
/// name of the account this process acts as.
#[cfg(target_os = "linux")]
fn read_passwd() -> Option<Account> {
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
    if !home.is_absolute() {
        return None;
    }
    // The login name may be absent (`pw_name` null) even when the home is there.
    let name = if passwd.pw_name.is_null() {
        std::ffi::OsString::new()
    } else {
        // SAFETY: `pw_name` is a NUL-terminated string inside `buffer`, alive.
        let name = unsafe { std::ffi::CStr::from_ptr(passwd.pw_name) };
        std::ffi::OsStr::from_bytes(name.to_bytes()).to_os_string()
    };
    Some(Account { home, name })
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
        for name in app_directory_names() {
            push_unique(&mut directories, base.join(name));
        }
    }
    directories
}

/// [`APP_DIRECTORY_NAME`] followed by the names builds from before the rename
/// gave the same directory, spelled for this platform the same way. A build
/// from before the rename keeps its settings there and can be the next one to
/// run, so they are protected and hidden alongside the current one.
fn app_directory_names() -> Vec<&'static str> {
    let mut names = vec![APP_DIRECTORY_NAME];
    for legacy in branding::LEGACY {
        let name = if cfg!(target_os = "linux") {
            legacy.app_directory_name_lowercase
        } else {
            legacy.app_directory_name
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// This fork's directory inside the config/data/state base directory the
/// environment variable `name` points at, or `None` when it is unset or a
/// relative path. [`remote_agent_runtime_data_directory`] uses it, and
/// [`app_state_directories_from_environment`] joins the same
/// [`APP_DIRECTORY_NAME`] onto the same bases (first among the names it
/// covers), so the readable check lands in the very directory the read hiding
/// covers.
#[cfg(target_os = "linux")]
fn app_state_directory_from_environment(name: &str) -> Option<PathBuf> {
    absolute_env_path(name).map(|base| base.join(APP_DIRECTORY_NAME))
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
            home.join(".forgekairos-claude"),
            home.join(".zed-jp-enhance-claude"),
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

    /// ADR-F49: a sandboxed Claude Code session reaches its own project's
    /// configuration through its write grant, and nothing else under the
    /// sessions' root — another project's login token stays hidden from it,
    /// and all of them from a command that works on an ordinary project.
    #[test]
    fn a_session_reaches_only_its_own_agent_configuration() {
        let home = tempfile::tempdir().expect("tempdir");
        let projects = home
            .path()
            .join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE)
            .join("projects");
        for project in ["own", "other"] {
            let config = projects.join(project).join("config");
            std::fs::create_dir_all(&config).expect("create");
            std::fs::write(config.join(".credentials.json"), "token").expect("write");
        }
        let own_grant = projects.join("own");

        let hidden = around_reachable(
            read_hidden_home_paths(home.path(), false),
            std::slice::from_ref(&own_grant),
        );
        assert!(hidden.contains(&projects.join("other")), "{hidden:?}");
        assert!(
            !hidden
                .iter()
                .any(|path| own_grant.join("config").starts_with(path)),
            "the session's own configuration stays reachable: {hidden:?}"
        );

        let worktree = home.path().join("work");
        let hidden = around_reachable(
            read_hidden_home_paths(home.path(), false),
            std::slice::from_ref(&worktree),
        );
        assert!(
            hidden.contains(&home.path().join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE)),
            "{hidden:?}"
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

    /// ADR-F49 decision 14: only what lies, once resolved, inside an agent
    /// installation or Zed's Node.js may be shown. The rest of Zed's data, a
    /// credential store, a directory holding one, a symlink leading out of the
    /// allowed directories and a path that is not there are all refused.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn only_an_agent_installation_or_node_may_be_shown() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let data = home.join(DATA_DIRECTORY_HOME_RELATIVE);
        let agent = data
            .join("external_agents")
            .join("registry")
            .join("npx")
            .join("claude");
        let node = data.join("node").join("node-v22.0.0");
        for directory in [
            &agent,
            &node,
            &data.join("db"),
            &data.join("external_agents_old"),
            &home.join(".ssh"),
        ] {
            std::fs::create_dir_all(directory).expect("create");
        }
        std::os::unix::fs::symlink(home.join(".ssh"), agent.join("keys")).expect("symlink");
        std::os::unix::fs::symlink(data.join("db"), node.join("db")).expect("symlink");

        for allowed in [
            agent.clone(),
            node.clone(),
            data.join("external_agents"),
            data.join("node"),
        ] {
            let resolved = resolve_readable_path(&allowed, &data, &home);
            assert!(
                resolved.is_ok(),
                "{} must be allowed: {resolved:?}",
                allowed.display()
            );
        }
        assert_eq!(
            resolve_readable_path(&agent.join("..").join("claude"), &data, &home).expect("allowed"),
            agent,
            "the resolved path is what is shown"
        );

        for refused in [
            data.clone(),
            data.join("db"),
            data.join("external_agents_old"),
            data.join("external_agents").join("..").join("db"),
            home.join(".ssh"),
            home.clone(),
            PathBuf::from("/"),
            agent.join("keys"),
            node.join("db"),
        ] {
            assert!(
                matches!(
                    resolve_readable_path(&refused, &data, &home),
                    Err(ReadableRefusal::OutsideAgentRuntime { .. })
                ),
                "{} must be refused",
                refused.display()
            );
        }
        assert!(matches!(
            resolve_readable_path(&agent.join("missing"), &data, &home),
            Err(ReadableRefusal::Unresolvable(_))
        ));
    }

    /// An `external_agents` or `node` that is itself a link to the data
    /// directory — or to anything holding it — shows nothing: the allowed
    /// directories are named below the data directory and never resolved
    /// themselves, so what the link leads to simply does not lie in one.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_link_where_an_agent_installation_belongs_shows_nothing() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let data = home.join(DATA_DIRECTORY_HOME_RELATIVE);
        std::fs::create_dir_all(data.join("db")).expect("create");
        std::os::unix::fs::symlink(&data, data.join("external_agents")).expect("symlink");
        std::os::unix::fs::symlink(&home, data.join("node")).expect("symlink");

        for refused in [
            data.join("external_agents"),
            data.join("external_agents").join("db"),
            data.join("node"),
            data.join("node").join(".ssh"),
        ] {
            assert!(
                resolve_readable_path(&refused, &data, &home).is_err(),
                "{} must be refused",
                refused.display()
            );
        }
    }

    /// A data directory that really lies inside a credential store shows
    /// nothing of it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_data_directory_inside_a_hidden_entry_shows_nothing() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let inside_keys = home.join(".ssh").join("zed");
        std::fs::create_dir_all(inside_keys.join("node")).expect("create");
        let data = home.join(DATA_DIRECTORY_HOME_RELATIVE);
        std::fs::create_dir_all(data.parent().expect("a parent")).expect("create");
        std::os::unix::fs::symlink(&inside_keys, &data).expect("symlink");

        assert!(matches!(
            resolve_readable_path(&data.join("node"), &data, &home),
            Err(ReadableRefusal::TouchesHiddenEntry { .. })
        ));
    }

    /// What a command may be shown it may never write: the data directory
    /// holding it is always protected, locally and by the remote helper.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn what_may_be_shown_is_inside_a_protected_entry() {
        let home = PathBuf::from("/home/u");
        let data = home.join(DATA_DIRECTORY_HOME_RELATIVE);
        assert!(
            local_builtin_protected_paths(&home, &[])
                .iter()
                .any(|protected| data.starts_with(protected)),
            "the local sandbox protects {}",
            data.display()
        );
        #[cfg(target_os = "linux")]
        assert!(
            remote_builtin_protected_paths(&home)
                .iter()
                .any(|protected| data.starts_with(protected)),
            "the remote helper protects {}",
            data.display()
        );
    }

    /// ADR-F49 decisions 14 and 15: the agents' configuration directory and the
    /// data directory are covered only around what the command was granted
    /// inside them, and only where they really are.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_enclosure_is_covered_only_around_what_is_granted_in_it() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let configurations = home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
        let own = home
            .join(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE)
            .join("0123456789abcdef")
            .join("config");
        let data = home.join(DATA_DIRECTORY_HOME_RELATIVE);
        let agent = data.join("external_agents").join("claude");
        std::fs::create_dir_all(&own).expect("create");
        std::fs::create_dir_all(&agent).expect("create");
        let work = home.join("work");
        std::fs::create_dir(&work).expect("create");

        let enclosures = enclosed_directories(&home);
        assert_eq!(enclosures, vec![configurations.clone(), data.clone()]);
        assert_eq!(
            enclosures_to_cover(&enclosures, &[&work, &own]),
            std::slice::from_ref(&configurations)
        );
        assert_eq!(
            enclosures_to_cover(&enclosures, &[&work, &own, &agent]),
            vec![configurations.clone(), data],
            "the data directory is covered around what may be read in it"
        );
        assert_eq!(
            enclosures_to_cover(&enclosures, &[&home, &own]),
            std::slice::from_ref(&configurations),
            "a root above it does not stop the cover"
        );
        assert!(enclosures_to_cover(&enclosures, &[&work]).is_empty());
        assert!(enclosures_to_cover(&enclosures, &[&configurations]).is_empty());

        let moved = home.join("moved");
        std::fs::rename(&configurations, &moved).expect("move it away");
        std::os::unix::fs::symlink(&moved, &configurations).expect("symlink");
        assert!(
            enclosures_to_cover(&enclosures, &[&configurations.join("projects")]).is_empty(),
            "a symlink in its place is not covered"
        );
    }

    /// ADR-F49 decisions 8 and 15: the agents' configuration directory is
    /// protected like any other credential store — a hook written in another
    /// project's directory runs in that project's next session — and steps
    /// aside only for the session that writes its own, whose cover protects
    /// the rest of it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_agents_configuration_directory_is_protected_unless_it_is_covered() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let configurations = home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
        let own = home
            .join(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE)
            .join("0123456789abcdef")
            .join("config");
        std::fs::create_dir_all(&own).expect("create");
        let work = home.join("work");
        std::fs::create_dir(&work).expect("create");

        assert!(
            builtin_protected_paths(&home, std::slice::from_ref(&work)).contains(&configurations),
            "an ordinary command may not write any of it"
        );
        assert!(
            !builtin_protected_paths(&home, &[work, own]).contains(&configurations),
            "the session that writes its own is left to the cover around it"
        );
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

    /// ADR-F49 decision 8 (2026-09-15): everything decision 11 hides from a
    /// sandboxed command's reads is also kept out of its writes.
    ///
    /// The two lists used to drift: `~/.npmrc`, `~/.cargo/credentials.toml`,
    /// `~/.config/gcloud` and the browser profiles were hidden but writable.
    /// That was invisible on the backends whose hiding is a read-only cover
    /// (the cover stopped the write too), and open on the Landlock one
    /// (decision 28), where read rules and write rules are separate — so a
    /// worktree that is the home let a command point the user's own next
    /// `npm install` at a registry of its choosing.
    ///
    /// Checked by prefix, since an entry below a protected directory is
    /// protected by it.
    ///
    /// Only for the Unix homes, where the two lists really are the same list:
    /// the Windows home is covered separately below.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn nothing_hidden_from_reads_is_left_writable() {
        let home = PathBuf::from("/home/u");
        let mut protected = local_builtin_protected_paths(&home, &[]);
        // The sandboxed agents' configuration directory: on both the local
        // backends and the helper it is protected around the one session's own
        // grant rather than flat, which `builtin_protected_paths` and
        // `run_helper` each do for themselves.
        protected.push(home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE));
        let hidden = read_hidden_home_paths(&home, false)
            .into_iter()
            .chain(resolve_home_relative(&home, LOCAL_READ_HIDDEN_EXTRA));
        for hidden in hidden {
            assert!(
                protected.iter().any(|entry| hidden.starts_with(entry)),
                "{} is hidden from reads but writable",
                hidden.display()
            );
        }
    }

    /// The same for a Windows home, where the hiding is always a read-only
    /// cover inside WSL — decision 28's Landlock plan is never made on a WSL
    /// kernel — so an entry inside a write grant is already unwritable through
    /// the cover, and the write list only has to name what a Windows tool
    /// really keeps there.
    ///
    /// All of `AppData` is hidden and deliberately not protected flat:
    /// protecting it would refuse every worktree below it, and the entries in
    /// it that matter are named one by one.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_windows_home_protects_the_credential_stores_it_really_holds() {
        let home = PathBuf::from(r"C:\Users\u");
        let protected = local_builtin_protected_paths(&home, &[]);
        for relative in [
            ".ssh",
            ".npmrc",
            ".yarnrc.yml",
            ".pypirc",
            r".cargo\credentials",
            r".cargo\credentials.toml",
            r".config\gcloud",
            r"AppData\Roaming\gcloud",
            ".terraform.d",
            ".password-store",
            r".config\gh",
            ".git-credentials",
        ] {
            let path = home.join(relative);
            assert!(
                protected.iter().any(|entry| path.starts_with(entry)),
                "{} is hidden from reads but writable",
                path.display()
            );
        }
        assert!(
            !protected.contains(&home.join("AppData")),
            "all of AppData is hidden from reads, but protecting it flat would \
             refuse every worktree below it"
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
            for name in ["ForgeKairos", "Zed-JP-Enhance"] {
                assert!(
                    contains(&format!(r"AppData\Roaming\{name}"))
                        && contains(&format!(r"AppData\Local\{name}")),
                    "this fork's own settings/state directory, under {name}"
                );
            }
            assert!(contains(".wslconfig"), "the sandbox's own WSL substrate");
        } else {
            assert!(contains(".bashrc"), "shell init");
            for (lowercase, name) in [
                ("forgekairos", "ForgeKairos"),
                ("zed-jp-enhance", "Zed-JP-Enhance"),
            ] {
                assert!(
                    contains(&format!(".config/{lowercase}"))
                        || contains(&format!("Library/Application Support/{name}")),
                    "this fork's own settings/state directory, under {name}"
                );
            }
        }
    }

    /// ADR-F49 decision 19 (8c): this fork's configuration folder — where the
    /// user's settings say an agent starts outside its sandbox every time — is
    /// protected whatever the project is: opened as the home itself, the folder
    /// is inside the write grant and still may not be written; the folder a
    /// `--user-data-dir` put elsewhere is protected as well. A project that
    /// does not hold it gains no protected entry inside it from this.
    #[test]
    fn the_forks_configuration_stays_protected_whatever_the_project() {
        let home = if cfg!(target_os = "windows") {
            PathBuf::from(r"C:\Users\u")
        } else {
            PathBuf::from("/home/u")
        };
        let configurations = if cfg!(target_os = "windows") {
            [
                r"AppData\Roaming\ForgeKairos",
                r"AppData\Roaming\Zed-JP-Enhance",
            ]
        } else if cfg!(target_os = "macos") {
            [
                "Library/Application Support/ForgeKairos",
                "Library/Application Support/Zed-JP-Enhance",
            ]
        } else {
            [".config/forgekairos", ".config/zed-jp-enhance"]
        }
        .map(|relative| home.join(relative));

        let home_as_project = builtin_protected_paths(&home, std::slice::from_ref(&home));
        let elsewhere = home.join("src").join("app");
        let ordinary = builtin_protected_paths(&home, std::slice::from_ref(&elsewhere));
        for configuration in &configurations {
            assert!(
                home_as_project.contains(configuration),
                "{home_as_project:?}"
            );
            assert!(ordinary.contains(configuration), "{ordinary:?}");
        }
        let inside_the_project = ordinary
            .iter()
            .filter_map(|path| path.strip_prefix(&elsewhere).ok())
            .collect::<Vec<_>>();
        assert!(
            inside_the_project.iter().all(|relative| {
                WORKTREE_PROTECTED_RELATIVE
                    .iter()
                    .any(|entry| *relative == Path::new(entry))
            }),
            "only the agent configurations of the project itself: {inside_the_project:?}"
        );

        let user_data_dir = if cfg!(target_os = "windows") {
            PathBuf::from(r"D:\portable\zed\config")
        } else {
            PathBuf::from("/opt/portable/zed/config")
        };
        assert!(
            local_builtin_protected_paths(&home, std::slice::from_ref(&user_data_dir))
                .contains(&user_data_dir)
        );
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
        let paths = worktree_protected_paths(
            Path::new("/home/u"),
            &[first.clone(), second.clone(), first.clone()],
        );
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

    /// ADR-F49 decision 15: a writable root that really lies inside the
    /// sandboxed agents' projects directory gets none of the worktree entries;
    /// a worktree, the projects directory itself, and a symlink in it that
    /// leads to a worktree still do.
    #[test]
    fn an_agent_configuration_directory_gets_no_worktree_entries() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let mut projects = home.clone();
        projects.extend(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE.split('/'));
        let configuration = projects.join("0123456789abcdef").join("config");
        let work = home.join("work");
        std::fs::create_dir_all(&configuration).expect("create");
        std::fs::create_dir_all(&work).expect("create");
        let roots = vec![configuration.clone(), work.clone(), projects.clone()];
        #[cfg(unix)]
        let roots = {
            let link = projects.join("link");
            std::os::unix::fs::symlink(&work, &link).expect("symlink");
            let mut roots = roots;
            roots.push(link);
            roots
        };

        let paths = worktree_protected_paths(&home, &roots);
        assert!(
            !paths.iter().any(|path| path.starts_with(&configuration)),
            "{paths:?}"
        );
        assert!(paths.contains(&work.join(".claude")), "{paths:?}");
        assert!(paths.contains(&projects.join(".claude")), "{paths:?}");
        #[cfg(unix)]
        assert!(
            paths.contains(&projects.join("link").join(".claude")),
            "{paths:?}"
        );
        assert!(
            builtin_protected_paths(&home, std::slice::from_ref(&configuration))
                .iter()
                .all(|path| !path.starts_with(&configuration))
        );
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

    /// The entries of `directory` in a tree of `(directory, [(name, where it
    /// really lies)])`, so the carve can be checked without a filesystem. A
    /// directory the tree does not name cannot be listed.
    fn listed(tree: &[(&str, &[(&str, &str)])], directory: &Path) -> Option<Vec<PathBuf>> {
        let entries = tree
            .iter()
            .find(|(listed, _)| Path::new(listed) == directory)?
            .1;
        Some(
            entries
                .iter()
                .map(|(_, resolved)| PathBuf::from(resolved))
                .collect(),
        )
    }

    const TREE: &[(&str, &[(&str, &str)])] = &[
        (
            "/",
            &[
                // Two names of one location, as a merged-`/usr` system has.
                ("bin", "/usr/bin"),
                ("sbin", "/usr/bin"),
                ("etc", "/etc"),
                ("home", "/home"),
                ("tmp", "/tmp"),
                ("usr", "/usr"),
            ],
        ),
        ("/home", &[("u", "/home/u"), ("other", "/home/other")]),
        (
            "/home/u",
            &[
                (".ssh", "/home/u/.ssh"),
                (".bashrc", "/home/u/.bashrc"),
                (".config", "/home/u/.config"),
                ("project", "/home/u/project"),
                // A link out of the home, and one back into the entry that
                // stays hidden.
                ("shared", "/srv/shared"),
                ("keys", "/home/u/.ssh"),
            ],
        ),
        (
            "/home/u/.config",
            &[
                ("gh", "/home/u/.config/gh"),
                ("nvim", "/home/u/.config/nvim"),
            ],
        ),
    ];

    /// ADR-F49 decision 11 where the covers are refused: the carve grants the
    /// whole filesystem except the way down to each hidden entry, which is what
    /// keeps `~/.ssh` unreadable — and what costs the listing of every
    /// directory on that way down.
    #[test]
    fn the_read_carve_grants_everything_but_the_way_down_to_a_hidden_entry() {
        let denied = [
            PathBuf::from("/home/u/.ssh"),
            PathBuf::from("/home/u/.config/gh"),
        ];
        let grants = carve_read_grants(Path::new("/"), &denied, &mut |directory| {
            listed(TREE, directory)
        });

        let granted = |path: &str| grants.iter().any(|grant| grant == Path::new(path));
        assert!(granted("/usr") && granted("/etc") && granted("/tmp"));
        assert!(
            granted("/home/other") && granted("/home/u/project") && granted("/home/u/.bashrc"),
            "{grants:?}"
        );
        assert!(
            granted("/home/u/.config/nvim"),
            "a sibling of a hidden entry is granted on its own"
        );
        assert!(
            !granted("/")
                && !granted("/home")
                && !granted("/home/u")
                && !granted("/home/u/.config"),
            "no ancestor of a hidden entry may be granted, or the entry comes with it"
        );
        assert!(
            !granted("/home/u/.ssh") && !granted("/home/u/.config/gh"),
            "{grants:?}"
        );
        assert!(
            !granted("/srv/shared"),
            "a link out of a carved directory is left to where it really lies, \
             which the carve grants at the level it belongs to"
        );
        assert_eq!(
            grants
                .iter()
                .filter(|grant| *grant == Path::new("/usr/bin"))
                .count(),
            1,
            "two names of one location must not grant it twice"
        );
    }

    /// A directory that cannot be listed grants nothing below it: the command
    /// does without those reads rather than the hidden entry being handed out
    /// with its parent.
    #[test]
    fn a_directory_that_cannot_be_listed_grants_nothing_below_it() {
        let tree: &[(&str, &[(&str, &str)])] = &[
            ("/", &[("home", "/home")]),
            ("/home", &[("u", "/home/u")]),
            // `/home/u` is missing from the tree, so it cannot be listed.
        ];
        let grants = carve_read_grants(
            Path::new("/"),
            &[PathBuf::from("/home/u/.ssh")],
            &mut |directory| listed(tree, directory),
        );
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// With nothing to hide the whole tree is granted with one rule, and a
    /// carve whose root is itself hidden grants nothing at all.
    #[test]
    fn the_read_carve_is_one_rule_when_nothing_is_hidden() {
        assert_eq!(
            carve_read_grants(Path::new("/"), &[], &mut |directory| listed(
                TREE, directory
            )),
            [PathBuf::from("/")]
        );
        assert!(
            carve_read_grants(
                Path::new("/home/u"),
                &[PathBuf::from("/home")],
                &mut |directory| listed(TREE, directory)
            )
            .is_empty()
        );
    }

    /// What the lists protect and hide under this fork's names, spelled out
    /// rather than derived from `branding`, so that dropping one of them —
    /// the name before the rename above all, which an older build still uses
    /// — fails here.
    #[test]
    fn the_lists_hold_the_current_and_the_earlier_names() {
        for entry in [
            ".zed_server",
            ".local/share/forgekairos",
            ".local/share/zed-jp-enhance",
        ] {
            assert!(UNIX_PROTECTED_HOME_RELATIVE.contains(&entry), "{entry}");
        }
        for entry in [
            ".zed_server",
            ".forgekairos-claude",
            ".zed-jp-enhance-claude",
            ".config/forgekairos",
            ".config/zed-jp-enhance",
            ".local/share/forgekairos",
            ".local/share/zed-jp-enhance",
            ".local/state/forgekairos",
            ".local/state/zed-jp-enhance",
        ] {
            assert!(READ_HIDDEN_HOME_RELATIVE.contains(&entry), "{entry}");
        }
        #[cfg(target_os = "windows")]
        for entry in [
            r"AppData\Roaming\ForgeKairos",
            r"AppData\Local\ForgeKairos",
            r"AppData\Roaming\Zed-JP-Enhance",
            r"AppData\Local\Zed-JP-Enhance",
        ] {
            assert!(LOCAL_PROTECTED_HOME_RELATIVE.contains(&entry), "{entry}");
        }
        #[cfg(target_os = "linux")]
        for entry in [
            ".config/forgekairos",
            ".config/zed-jp-enhance",
            ".local/state/forgekairos",
            ".local/state/zed-jp-enhance",
            ".forgekairos-claude",
            ".zed-jp-enhance-claude",
        ] {
            assert!(
                LOCAL_EXTRA_PROTECTED_HOME_RELATIVE.contains(&entry),
                "{entry}"
            );
        }
        #[cfg(target_os = "macos")]
        for entry in [
            "Library/Application Support/ForgeKairos",
            "Library/Application Support/Zed-JP-Enhance",
            ".forgekairos-claude",
            ".zed-jp-enhance-claude",
        ] {
            assert!(
                LOCAL_EXTRA_PROTECTED_HOME_RELATIVE.contains(&entry),
                "{entry}"
            );
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        assert_eq!(
            [
                WSL_SANDBOX_HELPER_HOME_RELATIVE,
                LEGACY_WSL_SANDBOX_HELPER_HOME_RELATIVE
            ],
            [".cache/forgekairos", ".cache/zed-jp-enhance"]
        );
        assert_eq!(
            [
                SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
                LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE
            ],
            [".forgekairos-claude", ".zed-jp-enhance-claude"]
        );
    }

    /// Every set of names this build or an earlier one used is in the lists:
    /// a set added to `branding::LEGACY` has to be added to them as well.
    #[test]
    fn every_name_this_fork_has_used_is_protected_and_hidden() {
        for names in std::iter::once(&branding::CURRENT).chain(branding::LEGACY) {
            let lowercase = names.app_directory_name_lowercase;
            let data = format!(".local/share/{lowercase}");
            assert!(
                UNIX_PROTECTED_HOME_RELATIVE.contains(&data.as_str()),
                "{data}"
            );
            for hidden in [
                format!(".config/{lowercase}"),
                data.clone(),
                format!(".local/state/{lowercase}"),
                names.sandboxed_agent_config_home_relative.to_string(),
            ] {
                assert!(
                    READ_HIDDEN_HOME_RELATIVE.contains(&hidden.as_str()),
                    "{hidden}"
                );
            }
            assert!(
                [
                    SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
                    LEGACY_SANDBOXED_AGENT_CONFIG_HOME_RELATIVE
                ]
                .contains(&names.sandboxed_agent_config_home_relative)
            );
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            assert!(
                [
                    WSL_SANDBOX_HELPER_HOME_RELATIVE,
                    LEGACY_WSL_SANDBOX_HELPER_HOME_RELATIVE
                ]
                .contains(&names.remote_cache_home_relative)
            );
            #[cfg(target_os = "windows")]
            for directory in [
                format!(r"AppData\Roaming\{}", names.app_directory_name),
                format!(r"AppData\Local\{}", names.app_directory_name),
            ] {
                assert!(
                    LOCAL_PROTECTED_HOME_RELATIVE.contains(&directory.as_str()),
                    "{directory}"
                );
            }
        }
    }

    #[test]
    fn the_agent_configuration_under_the_earlier_name_is_moved_to_the_current_one() {
        let home = tempfile::tempdir().expect("tempdir");
        let legacy = home.path().join(".zed-jp-enhance-claude");
        let token = legacy
            .join("projects")
            .join("key")
            .join("config")
            .join(".credentials.json");
        std::fs::create_dir_all(token.parent().expect("a parent")).expect("create");
        std::fs::write(&token, "token").expect("write");

        assert_eq!(
            migrate_legacy_sandboxed_agent_configuration_in(home.path()).expect("moves"),
            LegacyAgentConfigurationMigration::Moved
        );
        let current = home.path().join(".forgekairos-claude");
        assert_eq!(
            std::fs::read_to_string(
                current
                    .join("projects")
                    .join("key")
                    .join("config")
                    .join(".credentials.json")
            )
            .expect("the token came along"),
            "token"
        );
        assert!(!legacy.exists());

        assert_eq!(
            migrate_legacy_sandboxed_agent_configuration_in(home.path()).expect("runs again"),
            LegacyAgentConfigurationMigration::AlreadyThere
        );
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            migrate_legacy_sandboxed_agent_configuration_in(empty.path()).expect("runs"),
            LegacyAgentConfigurationMigration::NothingToMove
        );
        assert!(!empty.path().join(".forgekairos-claude").exists());
    }

    /// With both there, the current one is what the agents use and the earlier
    /// one is left alone: neither replaced nor merged.
    #[test]
    fn the_agent_configuration_is_not_moved_over_a_current_one() {
        let home = tempfile::tempdir().expect("tempdir");
        let legacy = home.path().join(".zed-jp-enhance-claude");
        let current = home.path().join(".forgekairos-claude");
        std::fs::create_dir_all(&legacy).expect("create");
        std::fs::write(legacy.join("old"), "old").expect("write");
        std::fs::create_dir_all(&current).expect("create");
        std::fs::write(current.join("new"), "new").expect("write");

        assert_eq!(
            migrate_legacy_sandboxed_agent_configuration_in(home.path()).expect("runs"),
            LegacyAgentConfigurationMigration::AlreadyThere
        );
        assert!(legacy.join("old").exists());
        assert!(current.join("new").exists());
        assert!(!current.join("old").exists());
        assert!(!current.join(".zed-jp-enhance-claude").exists());
    }

    /// Only a real directory is moved: a file under the earlier name, or a
    /// symbolic link there to somewhere else, stays where it is and the
    /// current name is not created.
    #[test]
    fn only_a_real_directory_under_the_earlier_name_is_moved() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            home.path().join(".zed-jp-enhance-claude"),
            "not a directory",
        )
        .expect("write");
        assert_eq!(
            migrate_legacy_sandboxed_agent_configuration_in(home.path()).expect("runs"),
            LegacyAgentConfigurationMigration::NotOwnDirectory
        );
        assert!(!home.path().join(".forgekairos-claude").exists());

        #[cfg(unix)]
        {
            let home = tempfile::tempdir().expect("tempdir");
            let elsewhere = home.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).expect("create");
            std::fs::write(elsewhere.join("secret"), "secret").expect("write");
            std::os::unix::fs::symlink(&elsewhere, home.path().join(".zed-jp-enhance-claude"))
                .expect("symlink");
            assert_eq!(
                migrate_legacy_sandboxed_agent_configuration_in(home.path()).expect("runs"),
                LegacyAgentConfigurationMigration::NotOwnDirectory
            );
            assert!(std::fs::symlink_metadata(home.path().join(".forgekairos-claude")).is_err());
            assert!(
                std::fs::symlink_metadata(home.path().join(".zed-jp-enhance-claude"))
                    .expect("the link stays")
                    .file_type()
                    .is_symlink()
            );
        }
    }
}
