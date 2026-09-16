//! Cross-platform sandboxing for commands run on behalf of the agent.
//!
//! The public API is intentionally platform-neutral. Internally, macOS uses
//! Seatbelt (`sandbox-exec`), Linux uses Bubblewrap (`bwrap`), and Windows runs
//! the SSH-remote helper (ADR-F18) inside WSL (ADR-F46). Restricted-network
//! policies are enforced by an HTTP/HTTPS proxy (the `http_proxy` crate) that
//! this crate constructs and owns — in-process, or on Windows inside the WSL
//! helper; callers only describe intent via [`SandboxPolicy`].
//! That proxy also enforces the editor-wide ceiling published through
//! [`set_network_ceiling`], so a command's allowed hosts can only ever narrow
//! what the application as a whole is permitted to reach. An *unrestricted*
//! grant is no exception: while a ceiling is published it too runs through the
//! proxy, waiving only the sandbox's own checks.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    process::Output,
    sync::{Arc, RwLock},
};

use http_proxy::{Allowlist, DestinationPolicy, HostPattern, ProxyHandle};
#[cfg(any(target_os = "linux", test))]
use http_proxy::{DenyReason, PolicyDecision};
#[cfg(not(target_os = "windows"))]
use http_proxy::{ProxyConfig, ProxyEvent, UpstreamProxy};

#[cfg(target_os = "linux")]
mod linux_bubblewrap;

/// ADR-F18: the on-host sandbox helper for SSH-remote Linux agent commands. The
/// execution half is Linux-only (it is the remote host); the argv builder that
/// the client uses to invoke it is cross-platform and lives below.
#[cfg(target_os = "linux")]
mod linux_remote_helper;

#[cfg(target_os = "macos")]
mod macos_seatbelt;

#[cfg(target_os = "windows")]
mod windows_wsl;

/// ADR-F49: the IDE channel from inside the sandbox back to Zed — its frame
/// protocol, Zed's demultiplexer, and on Linux the bridge process itself.
pub mod ide_bridge;
pub use ide_bridge::{
    BridgeConnection, BridgeConnections, BridgeFrame, BridgeFrameDecoder, BridgeFrameKind,
    BridgeMuxHandle, IdeBridgeSlot, MAX_BRIDGE_CONNECTIONS, MAX_BRIDGE_FRAME_PAYLOAD,
    MAX_IDE_BRIDGES, MAX_IDE_LOCK_CONTENT, encode_frame, encode_ide_lock_preamble,
    spawn_bridge_mux,
};

/// ADR-F18: the locations every sandbox protects regardless of what the caller
/// asked for. Shared by [`Sandbox::new`] (the local backends) and the SSH-remote
/// helper.
mod protected_paths;
pub use protected_paths::{
    APP_DIRECTORY_NAME, SANDBOXED_AGENT_CONFIG_HOME_RELATIVE,
    SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE, UNIX_PROTECTED_HOME_RELATIVE,
    WORKTREE_PROTECTED_RELATIVE, builtin_protected_paths, builtin_read_hidden_paths,
};

/// ADR-F49 decision 18: what still runs inside a sandbox, as the trusted side
/// outside it sees it.
pub mod sandbox_processes;
pub use sandbox_processes::{
    SANDBOX_KILL_FLAG, SANDBOX_PROCESSES_FAILURE_MARKER, SANDBOX_PROCESSES_FLAG, SandboxProcess,
    SandboxProcessKey, decode_sandbox_processes, encode_sandbox_processes,
    validate_sandbox_session_id,
};

mod util;

#[cfg(target_os = "macos")]
use util::canonicalize_allowing_missing_leaf;
#[cfg(target_os = "linux")]
use util::{CanonicalPathBuf, linux_fd_identity};
pub use util::{
    HostFilesystemLocation, HostFilesystemLocationDisplay, normalize_host_filesystem_locations,
    resolve_canonical,
};
#[cfg(target_os = "windows")]
pub use windows_wsl::{
    IDE_BRIDGE_READY_TIMEOUT, IdeBridge, IdeBridgeSessionRunning, ResolvedGrant, WslArchiveKind,
    WslSandboxUnavailable, WslSessionDistro, ensure_wsl_agent_config_dir, ide_bridge_invocation,
    ide_rendezvous_path, install_npm_package_in_wsl, is_forwardable_env_var,
    list_wsl_sandbox_processes, read_wsl_npm_package_json, registered_wsl_distro_name,
    resolve_canonical_for_grant, stage_agent_binary, stage_wsl_runtime_archive,
    stage_wsl_runtime_file, staged_agent_binary, staged_wsl_runtime, start_ide_bridge,
    stop_wsl_sandbox_processes, sweep_ide_rendezvous, wsl_agent_runtime_data_directory,
    wsl_distro_for_path, wsl_machine_architecture, wsl_npm_package_executable,
    wsl_runtime_executable,
};

#[cfg(target_os = "windows")]
pub(crate) const WSL_SANDBOX_UNAVAILABLE_PREFIX: &str =
    "WSL でサンドボックスを作成できませんでした";

/// A path *inside the sandbox* — i.e. where a host location is exposed in the
/// sandboxed process's view of the filesystem (for example, a bind-mount
/// destination on Linux).
///
/// Unlike [`HostFilesystemLocation`], this needs no hardening and is just a thin
/// wrapper around a [`PathBuf`]. It only names a location within the sandbox's
/// own namespace: the worst a tampered sandbox-side path can do is expose the
/// (already-granted) host files at a *different* path inside the sandbox — it can
/// never widen which host files are reachable. It is therefore fine to build one
/// from an ordinary, even attacker-influenced, path.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SandboxFilesystemLocation(PathBuf);

impl SandboxFilesystemLocation {
    /// Name a location inside the sandbox's filesystem view.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// The in-sandbox path. Safe to read: this is not a trusted host identity.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Consume this wrapper, yielding the underlying in-sandbox path.
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl From<PathBuf> for SandboxFilesystemLocation {
    fn from(path: PathBuf) -> Self {
        Self(path)
    }
}

/// What a command is allowed to do, expressed as intent. This is the entire
/// public configuration surface; how each policy is enforced (Seatbelt rules,
/// Bubblewrap flags, a loopback proxy, …) is an implementation detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxPolicy {
    pub fs: SandboxFsPolicy,
    pub network: SandboxNetPolicy,
}

/// Filesystem policy for a sandboxed command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxFsPolicy {
    /// Allow unrestricted filesystem writes except for protected paths, which
    /// remain readable but not writable.
    Unrestricted {
        protected_paths: Vec<HostFilesystemLocation>,
    },
    /// Reads are allowed everywhere; writes are confined to these locations
    /// (and the standard ephemeral locations the platform provides). Each is a
    /// [`HostFilesystemLocation`] captured at validation time, never a bare path
    /// the enforcement layer would re-resolve.
    Restricted {
        writable_paths: Vec<HostFilesystemLocation>,
        protected_paths: Vec<HostFilesystemLocation>,
        /// ADR-F49 decision 14: locations the command may read although they
        /// lie among the entries no sandboxed command may read (decision 11) —
        /// read, never write. What an agent needs to run: its installation and
        /// the Node.js Zed runs it with, both in Zed's data directory, which is
        /// hidden as a whole otherwise.
        ///
        /// Only a location inside the `external_agents` or `node` directory of
        /// Zed's data directory under the account's home may be named. Anything
        /// else — a credential store or a directory holding one, Zed's
        /// database, settings or logs, the remote server itself — refuses the
        /// whole command ([`SandboxError::InvalidRequest`] here, a fail-closed
        /// `readable-refused` from the remote helper) instead of being dropped.
        /// Decided on the resolved path, so a symlink leading anywhere else is
        /// refused too, and never from the environment. Where the read hiding
        /// cannot be applied at all (an SSH host that refuses the mounts), there
        /// is nothing to lift and these change nothing.
        readable_paths: Vec<HostFilesystemLocation>,
    },
}

/// Outbound-network policy for a sandboxed command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxNetPolicy {
    /// Impose no host restriction of the sandbox's *own* — the command may
    /// reach anything the editor as a whole may reach.
    ///
    /// This is not an exemption from the editor-wide ceiling
    /// ([`set_network_ceiling`]). While one is published the command still runs
    /// through the proxy, with an allow-everything base decision (IP literals
    /// and loopback included), so the ceiling is the only policy left deciding.
    /// With no ceiling published there is nothing left to enforce and the
    /// command gets the raw network with no proxy in front of it.
    Unrestricted,
    /// Block all outbound network access.
    Blocked,
    /// Allow outbound HTTP(S) only to these hostnames (exact hosts or
    /// leading-`*.` subdomain wildcards), enforced by an in-process proxy.
    Restricted { allowed_domains: Vec<String> },
}

/// The editor-wide network policy that every sandboxed command's own host
/// allowlist sits inside.
///
/// A sandboxed command reaches a host only when
/// [`SandboxNetPolicy::Restricted`] *and* this policy both permit it — the
/// hosts approved for one command narrow what the editor as a whole may reach,
/// they never widen it. [`SandboxNetPolicy::Unrestricted`] is the degenerate
/// case of the same rule, not a way out of it: the command's own half of the
/// decision permits everything, so while a ceiling is published the command is
/// still proxied and this policy alone decides. `None` (the default) leaves the
/// per-command allowlist as the only check and an unrestricted grant genuinely
/// unrestricted, which is what the sandbox test helpers and any embedder with
/// no editor-wide policy get.
///
/// A process-wide slot rather than a field on [`SandboxPolicy`]: the ceiling is
/// a property of the running application, identical for every command, and this
/// crate deliberately knows nothing about the crate that owns it (`egress_policy`,
/// which reads and watches `network_allowlist.json`). Same arrangement, and for
/// the same reason, as `http_client::set_child_process_proxy`.
static NETWORK_CEILING: RwLock<Option<Arc<dyn DestinationPolicy>>> = RwLock::new(None);

/// Publishes (or, with `None`, withdraws) the [`NETWORK_CEILING`]. Called once
/// at startup by the crate that owns the editor's egress allowlist. The policy
/// object is expected to read a live handle, so later edits to that allowlist
/// apply to the next connection without anything being re-published here.
pub fn set_network_ceiling(policy: Option<Arc<dyn DestinationPolicy>>) {
    // A poisoned lock means a previous writer panicked while holding it; the
    // slot itself is a single `Option`, so there is no torn state to recover
    // from and overwriting it is exactly right.
    *NETWORK_CEILING
        .write()
        .unwrap_or_else(|error| error.into_inner()) = policy;
}

/// Whether this process has published a ceiling at all.
///
/// For a caller whose grant *is* the ceiling — "this command may reach whatever
/// the editor may", which is [`SandboxNetPolicy::Unrestricted`] plus a ceiling
/// to do the deciding — the question has to be asked before the policy is
/// built: those same words in a process that published none (an embedder, a
/// test) say "no restriction at all", which is the opposite of what was meant.
pub fn network_ceiling_published() -> bool {
    network_ceiling().is_some()
}

fn network_ceiling() -> Option<Arc<dyn DestinationPolicy>> {
    NETWORK_CEILING
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Where the running application keeps its own data and configuration.
///
/// The sandbox has to keep a command out of the editor's own directories — its
/// settings, its thread database, its logs — and has to recognise, inside the
/// data directory, the agents the editor installed and the Node.js it runs them
/// with as the one thing a sandboxed agent may be shown there (ADR-F49
/// decisions 11 and 14). The default places are home-relative and this crate
/// knows them, but `--user-data-dir`, `XDG_DATA_HOME` and the Flatpak variables
/// move them, and this crate must not read the environment to decide what stays
/// visible: a variable a project can set would be a way to un-hide a credential
/// store. So the application registers them, exactly as it publishes the
/// network ceiling; what the environment names is still *added* to the
/// protected and hidden sets, which can only take access away.
///
/// A process-wide slot rather than a field on [`SandboxPolicy`]: these are a
/// property of the running application, identical for every command. Nothing is
/// registered in the remote helper (it is not the editor) or in an embedder
/// that never calls [`register_application_directories`], and the home-relative
/// defaults are then all the sandbox knows.
static APPLICATION_DIRECTORIES: RwLock<Option<ApplicationDirectories>> = RwLock::new(None);

#[derive(Clone)]
struct ApplicationDirectories {
    data: PathBuf,
    config: PathBuf,
}

/// Register the directories this process really uses (see
/// [`APPLICATION_DIRECTORIES`]). Called once at startup, from `main`, after
/// `--user-data-dir` has been applied and before any sandbox is built.
pub fn register_application_directories(data_directory: PathBuf, config_directory: PathBuf) {
    // A poisoned lock means a previous writer panicked while holding it; the
    // slot is a single `Option`, so there is no torn state to recover from.
    *APPLICATION_DIRECTORIES
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(ApplicationDirectories {
        data: data_directory,
        config: config_directory,
    });
}

/// Both registered directories, for the protected and read-hidden sets. Empty
/// where nothing was registered.
pub(crate) fn registered_application_directories() -> Vec<PathBuf> {
    APPLICATION_DIRECTORIES
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|directories| vec![directories.data.clone(), directories.config.clone()])
        .unwrap_or_default()
}

/// The registered data directory alone: where an agent installation and Zed's
/// Node.js are looked for (ADR-F49 decision 14).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn registered_data_directory() -> Option<PathBuf> {
    APPLICATION_DIRECTORIES
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|directories| directories.data.clone())
}

/// ADR-F46: the editor-wide network allowlist as the WSL helper's proxy
/// enforces it around the command's own host list. The decision is the one
/// `egress_policy` makes for the in-process proxy on macOS and Linux (same
/// matching, same loopback exemption, same refusal), taken from the entries the
/// Windows side captured when it wrapped the command.
#[cfg(any(target_os = "linux", test))]
pub(crate) struct EditorAllowlistCeiling {
    policy: http_client::EgressPolicy,
}

#[cfg(any(target_os = "linux", test))]
impl EditorAllowlistCeiling {
    /// A malformed entry is refused rather than skipped: the Windows side only
    /// sends entries that already parsed there, so one that doesn't here means
    /// the two sides disagree about the allowlist.
    pub(crate) fn parse(entries: &[String]) -> anyhow::Result<Self> {
        let (policy, errors) = http_client::EgressPolicy::from_host_strings(entries);
        if let Some(error) = errors.first() {
            anyhow::bail!("invalid network allowlist entry: {error}");
        }
        Ok(Self { policy })
    }
}

#[cfg(any(target_os = "linux", test))]
impl DestinationPolicy for EditorAllowlistCeiling {
    fn evaluate(&self, host: &str, _port: u16) -> PolicyDecision {
        let permitted =
            http_client::RequestHost::parse(host).is_some_and(|host| self.policy.allows(&host));
        if permitted {
            // No objection to local addresses from this half; the command's
            // own decision, ANDed with it, still refuses them unless the
            // command was granted any host.
            return PolicyDecision::Allow {
                permit_local_addresses: true,
            };
        }
        PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
            host: host.to_string(),
            explanation: format!("host '{host}' is not on the network allowlist"),
        })
    }
}

/// Host paths that should remain readable but not writable, even when they fall
/// under a writable subtree.
///
/// The caller computes this list because the sandbox layer does not know which
/// application-specific paths need stronger protection.
pub type ProtectedPaths = Vec<HostFilesystemLocation>;

/// Whether `path` (a host path) resides on a Windows drive reached through WSL's
/// DrvFs, where sandbox-integrity guarantees are weaker than on the distro's
/// native filesystem. Always `false` off Windows, where no such filesystem
/// exists. This is a cheap structural check on the path shape; the authoritative
/// per-grant classification is done in WSL by [`resolve_canonical_for_grant`].
pub fn path_is_on_windows_drive(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_wsl::path_is_on_windows_drive(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        false
    }
}

/// Whether WSL is structurally present (at least one registered distro), via a
/// cheap registry read with no `wsl.exe` round-trip. Always `false` off
/// Windows. Suitable only for UI gating (e.g. suppressing DrvFs
/// sandbox-integrity warnings on machines where no WSL sandbox could ever be
/// constructed); enforcement paths must keep relying on the real in-WSL probe,
/// which is authoritative about whether a sandbox can actually be built.
pub fn wsl_distro_registered() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_wsl::wsl_distro_registered()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

impl SandboxPolicy {
    /// Combine two policy layers. Filesystem/network grants are unioned into the
    /// least-restrictive policy that satisfies both layers, while protected paths
    /// within restricted filesystem policies are unioned so every layer's
    /// protected subtrees remain protected.
    pub fn merge(self, other: SandboxPolicy) -> SandboxPolicy {
        SandboxPolicy {
            fs: self.fs.merge(other.fs),
            network: self.network.merge(other.network),
        }
    }

    /// Replace the protected paths in the filesystem policy, keeping writable
    /// paths and network policy unchanged.
    pub fn with_protected_paths(mut self, protected_paths: Vec<HostFilesystemLocation>) -> Self {
        match &mut self.fs {
            SandboxFsPolicy::Unrestricted {
                protected_paths: existing,
            }
            | SandboxFsPolicy::Restricted {
                protected_paths: existing,
                ..
            } => *existing = protected_paths,
        }
        self
    }
}

fn merge_locations(
    locations: Vec<HostFilesystemLocation>,
    other: Vec<HostFilesystemLocation>,
) -> Vec<HostFilesystemLocation> {
    // Union the two sets and reduce to a minimal cover, so a location nested
    // under another (in either input) is dropped rather than bound redundantly.
    normalize_host_filesystem_locations(locations.into_iter().chain(other))
}

fn validate_writable_paths_do_not_overlap_protected_paths(
    writable_paths: &[HostFilesystemLocation],
    protected_paths: &[HostFilesystemLocation],
) -> Result<(), SandboxError> {
    for writable_path in writable_paths {
        for protected_path in protected_paths {
            if writable_path_overlaps_protected_path(writable_path, protected_path) {
                return Err(SandboxError::InvalidRequest(format!(
                    "writable sandbox path `{}` overlaps protected path `{}`",
                    writable_path.untrusted_path_display(),
                    protected_path.untrusted_path_display()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn writable_path_overlaps_protected_path(
    writable_path: &HostFilesystemLocation,
    protected_path: &HostFilesystemLocation,
) -> bool {
    linux_location_is_equal_or_descendant(writable_path, protected_path)
}

#[cfg(target_os = "macos")]
fn writable_path_overlaps_protected_path(
    writable_path: &HostFilesystemLocation,
    protected_path: &HostFilesystemLocation,
) -> bool {
    writable_path
        .macos_canonical_path()
        .starts_with(protected_path.macos_canonical_path())
}

/// Windows: on the *canonical* paths, never the raw requests. Both sides are
/// normalized by `CanonicalPathBuf` (ADR-F49 第 2b 段), so the one folder an
/// ext4 worktree is — writable as `\\wsl$\Ubuntu\p` and protected as
/// `\\wsl.localhost\Ubuntu\p\.git`, say — compares as one folder here.
/// Comparing the raw requests instead let those two spellings read as
/// unrelated places and the overlap went unreported: not an escape (the helper
/// inside WSL resolves and enforces both), but the early, explanatory refusal
/// of a bad request was lost.
#[cfg(target_os = "windows")]
fn writable_path_overlaps_protected_path(
    writable_path: &HostFilesystemLocation,
    protected_path: &HostFilesystemLocation,
) -> bool {
    writable_path
        .windows_canonical_path()
        .starts_with(protected_path.windows_canonical_path())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn writable_path_overlaps_protected_path(
    writable_path: &HostFilesystemLocation,
    protected_path: &HostFilesystemLocation,
) -> bool {
    writable_path
        .untrusted_raw_path()
        .starts_with(protected_path.untrusted_raw_path())
}

#[cfg(target_os = "linux")]
fn linux_location_is_equal_or_descendant(
    location: &HostFilesystemLocation,
    ancestor: &HostFilesystemLocation,
) -> bool {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    if location == ancestor {
        return true;
    }

    let Ok(mut current) = location.linux_dup_fd() else {
        log::warn!("failed to duplicate sandbox location fd while checking protected path overlap");
        return false;
    };

    loop {
        let parent = unsafe {
            let fd = libc::openat(
                current.as_raw_fd(),
                c"..".as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return false;
            }
            OwnedFd::from_raw_fd(fd)
        };

        let Some(parent_identity) = linux_fd_identity(parent.as_raw_fd()) else {
            log::warn!("failed to fstat parent fd while checking protected path overlap");
            return false;
        };
        let Some(current_identity) = linux_fd_identity(current.as_raw_fd()) else {
            log::warn!("failed to fstat current fd while checking protected path overlap");
            return false;
        };
        let Some(ancestor_identity) = linux_fd_identity(ancestor.linux_fd().as_raw_fd()) else {
            log::warn!("failed to fstat protected fd while checking protected path overlap");
            return false;
        };

        if parent_identity == ancestor_identity {
            return true;
        }
        if parent_identity == current_identity {
            return false;
        }
        current = parent;
    }
}

/// Whether a built-in protected entry that does not exist yet can still be
/// protected on this platform.
///
/// Seatbelt matches a path *literal*, so a rule for a path that doesn't exist
/// yet still denies its creation — worth having, since creating `~/.bashrc` is
/// as good as rewriting it. Bubblewrap (native Linux, and inside WSL) can only
/// re-bind an object that exists, so there the backend drops such an entry
/// anyway; on Windows keeping it would additionally cost a `wslpath` round-trip
/// on every command, because missing resolutions are deliberately not cached.
const PROTECTS_NOT_YET_EXISTING_PATHS: bool = cfg!(target_os = "macos");

/// Add the built-in protected set ([`protected_paths`]) to the caller's list, so
/// a worktree that *is* the home directory can't be used to rewrite the
/// account's credentials, shell init files, `PATH` binaries, or this fork's own
/// settings.
///
/// This lives in the enforcement layer rather than in the callers that build a
/// policy because the agent reaches `Sandbox` by several routes (the terminal
/// tool, a thread's persisted grants, the status UI's probe) and any one of them
/// that forgot would be a silent hole. It is the local counterpart of what
/// `linux_remote_helper::run_helper` does on an SSH host, and follows the same
/// rules:
///
/// - an entry that isn't there is skipped, unless the backend can protect a
///   not-yet-existing path anyway (see [`PROTECTS_NOT_YET_EXISTING_PATHS`]);
/// - an entry that exists but cannot be captured fails the command **closed**: a
///   dropped protection is not a missing restriction, it is a granted one. A
///   *dangling symlink* counts as existing, because a write "through" it creates
///   its target — `~/.bashrc -> dotfiles/bashrc` pointing into the worktree
///   would be executed at the next login;
/// - an entry the caller already named is not added twice
///   ([`HostFilesystemLocation`] compares filesystem objects, not path text); and
/// - a writable grant that is the same object as, or inside, a built-in entry is
///   refused, exactly as an overlap with a caller-supplied protected path is.
fn add_builtin_protected_paths(
    requested: Vec<HostFilesystemLocation>,
    writable_paths: &[HostFilesystemLocation],
) -> Result<ProtectedSetup, SandboxError> {
    let Some(home) = protected_paths::home_dir_from_environment() else {
        return Err(SandboxError::Other(
            "ホームディレクトリを特定できないため、常に保護するパス (認証情報・シェル初期化\
             ファイル・設定ディレクトリ) を確定できません。サンドボックスを構築せずに中止します。"
                .to_string(),
        ));
    };
    let worktree_roots: Vec<PathBuf> = writable_paths
        .iter()
        .filter_map(writable_directory_path)
        .collect();
    let builtin_paths = protected_paths::builtin_protected_paths(&home, &worktree_roots);
    let requested_count = requested.len();
    let setup = merge_protected_paths(requested, writable_paths, builtin_paths)?;

    log::debug!(
        "[sandbox] protecting {} paths ({} built-ins under {}, {} of them not there yet)",
        setup.paths.len(),
        setup.paths.len() - requested_count,
        home.display(),
        setup.missing.len()
    );
    Ok(setup)
}

/// The protected set for one command: what exists and can be enforced object by
/// object, and what does not exist yet.
#[derive(Debug)]
struct ProtectedSetup {
    paths: Vec<HostFilesystemLocation>,
    /// ADR-F49 decision 8: built-in entries that are not there yet, where they
    /// would be created. Creating one is as good as writing it — a worktree's
    /// `.claude/settings.local.json` runs on the host at the next agent turn —
    /// so a backend that cannot protect a path that does not exist keeps the
    /// command from creating it instead (see [`carve_writable_binds`]).
    /// Empty where the backend protects them by path anyway
    /// ([`PROTECTS_NOT_YET_EXISTING_PATHS`]).
    missing: Vec<PathBuf>,
}

/// ADR-F49 decision 14: refuse the whole command when one of `readable_paths`
/// may not be shown (see [`protected_paths::resolve_readable_path`]), rather
/// than run it with that entry left out.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_readable_paths(readable_paths: &[HostFilesystemLocation]) -> Result<(), SandboxError> {
    if readable_paths.is_empty() {
        return Ok(());
    }
    let Some(home) = protected_paths::home_dir_from_environment() else {
        return Err(SandboxError::Other(
            "ホームディレクトリを特定できないため、読み取り専用で見せるパスを確認できません。\
             サンドボックスを構築せずに中止します。"
                .to_string(),
        ));
    };
    let data_directory = protected_paths::agent_runtime_data_directory(&home);
    check_readable_paths_for(readable_paths, &data_directory, &home)
}

/// Windows leaves the check to the helper inside WSL, where the paths are
/// resolved and where their home is; it fails the command closed the same way
/// (`readable-refused`).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_readable_paths(_readable_paths: &[HostFilesystemLocation]) -> Result<(), SandboxError> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn check_readable_paths_for(
    readable_paths: &[HostFilesystemLocation],
    data_directory: &Path,
    home: &Path,
) -> Result<(), SandboxError> {
    use protected_paths::ReadableRefusal;

    for location in readable_paths {
        let path = local_canonical_path(location);
        let refusal = match protected_paths::resolve_readable_path(path, data_directory, home) {
            Ok(canonical) if canonical == path => continue,
            Ok(canonical) => format!(
                "読み取り専用で見せるパス `{}` は、確認のあとで別の場所 (`{}`) を指すように\
                 置き換えられています。安全のため中止します。",
                location.untrusted_path_display(),
                canonical.display()
            ),
            Err(ReadableRefusal::Unresolvable(error)) => format!(
                "読み取り専用で見せるパス `{}` を確認できませんでした ({error})。\
                 確認できないものは見せないため、中止します。",
                location.untrusted_path_display()
            ),
            Err(ReadableRefusal::OutsideAgentRuntime {
                canonical,
                data_directory,
            }) => format!(
                "読み取り専用で見せるパス `{}` (実際の場所 `{}`) は見せられません。見せられるのは \
                 Zed のデータフォルダー (`{}`) の中のエージェントのインストール先 \
                 (`external_agents`) と Node.js (`node`) だけです \
                 (認証情報や Zed のデータベース・設定・ログは見せません)。",
                location.untrusted_path_display(),
                canonical.display(),
                data_directory.display()
            ),
            Err(ReadableRefusal::TouchesHiddenEntry { canonical, entry }) => format!(
                "読み取り専用で見せるパス `{}` (実際の場所 `{}`) は、読み取りを禁止している `{}` と\
                 重なるため見せられません。",
                location.untrusted_path_display(),
                canonical.display(),
                entry.display()
            ),
        };
        return Err(SandboxError::InvalidRequest(refusal));
    }
    Ok(())
}

/// Where a captured location lies, on the backends that enforce by path.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn local_canonical_path(location: &HostFilesystemLocation) -> &Path {
    #[cfg(target_os = "linux")]
    {
        location.linux_canonical_path()
    }
    #[cfg(target_os = "macos")]
    {
        location.macos_canonical_path()
    }
}

/// ADR-F49 decisions 11 and 15 on the local Bubblewrap and Seatbelt backends.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct LocalReadHiding {
    /// What the command may not read:
    /// [`protected_paths::builtin_read_hidden_paths`] split around the roots it
    /// may write and the locations it may read, canonical where an entry exists
    /// so a symlinked `~/.ssh` is hidden where it really lies, and never one
    /// that lands on either.
    hidden: Vec<PathBuf>,
    /// Directories covered whole around the roots written inside them (see
    /// [`protected_paths::enclosures_to_cover`]). What lies in one is left out
    /// of `hidden`: the cover hides it.
    enclosures: Vec<PathBuf>,
}

/// [`LocalReadHiding`] for the account's home. Nothing from the environment
/// decides what stays visible: a variable a project can set would be a way to
/// un-hide a credential store.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn local_read_hiding(
    writable: &[&Path],
    readable: &[&Path],
    allow_fs_write: bool,
) -> Result<LocalReadHiding, SandboxError> {
    let Some(home) = protected_paths::home_dir_from_environment() else {
        return Err(SandboxError::Other(
            "ホームディレクトリを特定できないため、読み取りを禁止するパス (認証情報・\
             エージェント設定) を確定できません。サンドボックスを構築せずに中止します。"
                .to_string(),
        ));
    };
    Ok(local_read_hiding_for_home(
        &home,
        writable,
        readable,
        allow_fs_write,
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn local_read_hiding_for_home(
    home: &Path,
    writable: &[&Path],
    readable: &[&Path],
    allow_fs_write: bool,
) -> LocalReadHiding {
    // Resolved, so that an entry holding a root is recognised as such and split
    // around it even when the home is reached through a symlink.
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    // Each location is used exactly as it was captured and checked — where it
    // led when the user approved it, and (for what may be read) where the check
    // above proved it lies. Resolving one again here would decide on what it
    // points at *now*: one symlink planted where a location was would take the
    // cover off whatever it leads to — `~/.ssh` — for every command that
    // follows, and the entry would stay uncovered for good.
    let reachable: Vec<PathBuf> = writable
        .iter()
        .chain(readable)
        .map(|path| path.to_path_buf())
        .collect();
    let granted: Vec<&Path> = writable.iter().chain(readable).copied().collect();
    let enclosures = if allow_fs_write {
        // A command that may write everywhere names no roots, so there is
        // nothing to bind back into a cover; the backends leave the covers out
        // as well, and the two must agree on that.
        Vec::new()
    } else {
        protected_paths::enclosures_to_cover(
            &protected_paths::enclosed_directories(&home),
            &granted,
        )
    };
    // Each entry resolved before it is split, so one reached through a symlink
    // (`~/.config` kept elsewhere) is split around a root inside it rather than
    // dropped whole by the last check.
    let entries: Vec<PathBuf> = protected_paths::builtin_read_hidden_paths(&home, &[])
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .collect();
    let hidden = protected_paths::around_reachable(entries, &reachable)
        .into_iter()
        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
        .filter(|path| {
            !enclosures
                .iter()
                .any(|enclosure| path.starts_with(enclosure))
        })
        .collect();
    LocalReadHiding {
        hidden: protected_paths::without_reachable(hidden, &reachable),
        enclosures,
    }
}

/// ADR-F49 decision 8: the writable binds for one command — each root as one
/// bind, or, when a built-in protected entry that is not there yet lies inside
/// it, the root's entries one by one.
///
/// Bubblewrap can only re-bind an object that exists, so a missing
/// `.claude/settings.local.json` in a worktree without a `.claude/` would
/// otherwise be creatable, and a host agent would run its hooks at the next
/// turn. Carving the root is what the remote helper's Landlock grants do for
/// the same reason (`plan_subtree`): the root directory itself stays read-only,
/// so nothing new can be created directly in it, while everything already in it
/// stays writable. Nothing is created on the host to achieve this — no
/// placeholder for the missing entry.
///
/// Each bind carries the descriptor the in-sandbox validator compares the
/// mounted inode against, so a child swapped for a symlink between the listing
/// here and the mount fails the command closed rather than binding whatever it
/// now leads to.
#[cfg(target_os = "linux")]
fn carve_writable_binds(
    writable_paths: &[HostFilesystemLocation],
    missing_protected_paths: &[PathBuf],
) -> Result<Vec<(PathBuf, std::os::fd::OwnedFd)>, SandboxError> {
    let mut binds = Vec::new();
    for location in writable_paths {
        let path = location.linux_canonical_path();
        let carve = missing_protected_paths
            .iter()
            .any(|target| target.starts_with(path) && target.as_path() != path);
        if carve {
            carve_directory(path, missing_protected_paths, &mut binds, 0)?;
            continue;
        }
        match location.linux_dup_fd() {
            Ok(fd) => binds.push((path.to_path_buf(), fd)),
            Err(error) => {
                // Fail closed: a bind we can't pin a verifiable fd for is
                // dropped rather than bound unverified.
                log::warn!(
                    "[sandbox] could not duplicate fd for writable bind {}: {error}",
                    path.display()
                );
            }
        }
    }
    Ok(binds)
}

/// One level of [`carve_writable_binds`]: bind each entry of `directory` that
/// leads to no carve target, and carve in turn the ones that do. `depth` is 0
/// for the root itself.
#[cfg(target_os = "linux")]
fn carve_directory(
    directory: &Path,
    carve_targets: &[PathBuf],
    binds: &mut Vec<(PathBuf, std::os::fd::OwnedFd)>,
    depth: usize,
) -> Result<(), SandboxError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // Below the root, a directory that cannot be listed is granted nothing
        // rather than failing the command: binding it whole is the one thing we
        // must not do, since that is what re-exposes the entry underneath. The
        // root itself is different — nothing sensible can be granted there.
        Err(error) if depth > 0 => {
            log::warn!(
                "[sandbox] granting no write under {} (cannot list it: {error})",
                directory.display()
            );
            return Ok(());
        }
        Err(error) => {
            return Err(SandboxError::Io(format!(
                "書込を許可したパス `{}` の中を読めませんでした ({error})。\
                 保護すべきパスを切り分けられないため、中止します。",
                directory.display()
            )));
        }
    };
    for entry in entries.filter_map(std::result::Result::ok) {
        let child = entry.path();
        // A symlink is left alone: binding it would follow it and hand write
        // access to wherever it leads. It cannot be replaced either — the
        // directory holding it is read-only inside the sandbox.
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        if carve_targets
            .iter()
            .any(|target| target.starts_with(&child))
        {
            carve_directory(&child, carve_targets, binds, depth + 1)?;
            continue;
        }
        match util::open_o_path(&child, util::FollowSymlinks::No) {
            Ok(fd) => binds.push((child, fd)),
            Err(error) => log::warn!(
                "[sandbox] granting no write on {} ({error})",
                child.display()
            ),
        }
    }
    Ok(())
}

/// Where a writable grant lies on the host when it is a directory — a worktree
/// root, which the built-in worktree entries are resolved against. On Windows a
/// grant on a distro's own filesystem is left to the helper there, which adds
/// those entries for every root it may write itself, missing ones included: a
/// `/…` path, and (ADR-F49 第 2b 段) a `\\wsl.localhost\<distro>\…` share, which
/// the Windows side must not resolve against the *Windows* home either.
fn writable_directory_path(location: &HostFilesystemLocation) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = location.linux_canonical_path();
    #[cfg(target_os = "macos")]
    let path = location.macos_canonical_path();
    #[cfg(target_os = "windows")]
    let path = location.windows_canonical_path();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let path = location.untrusted_raw_path();
    #[cfg(target_os = "windows")]
    {
        let text = path.to_string_lossy();
        if text.starts_with('/') || util::parse_wsl_share_path(&text).is_some() {
            return None;
        }
    }
    path.is_dir().then(|| path.to_path_buf())
}

/// The capture-and-merge half of [`add_builtin_protected_paths`], split out so
/// it can be exercised against a temporary directory instead of the real home.
fn merge_protected_paths(
    requested: Vec<HostFilesystemLocation>,
    writable_paths: &[HostFilesystemLocation],
    builtin_paths: Vec<PathBuf>,
) -> Result<ProtectedSetup, SandboxError> {
    let mut merged = requested;
    let mut missing_paths = Vec::new();
    for path in builtin_paths {
        let missing = std::fs::symlink_metadata(&path)
            .err()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if missing && !PROTECTS_NOT_YET_EXISTING_PATHS {
            if let Some(location) = missing_entry_location(&path) {
                push_unique_path(&mut missing_paths, location);
            }
            continue;
        }
        let location = match HostFilesystemLocation::capture(&path) {
            Ok(location) => location,
            Err(_) if missing => continue,
            Err(error) => {
                return Err(SandboxError::Other(format!(
                    "常に保護するパス `{}` を確認できませんでした ({error})。\
                     保護できないままコマンドを実行しないため、中止します。",
                    path.display()
                )));
            }
        };
        for writable_path in writable_paths {
            if writable_path_overlaps_protected_path(writable_path, &location) {
                return Err(SandboxError::InvalidRequest(format!(
                    "書込を許可したパス `{}` は常に保護するパス `{}` と重なります \
                     (認証情報・シェル初期化ファイル・設定ディレクトリへの書込は許可できません)。",
                    writable_path.untrusted_path_display(),
                    location.untrusted_path_display()
                )));
            }
        }
        if merged.contains(&location) {
            continue;
        }
        merged.push(location);
    }
    Ok(ProtectedSetup {
        paths: merged,
        missing: missing_paths,
    })
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Where an entry that is not there yet would be created: its deepest existing
/// ancestor resolved, with the rest of the name as written. `None` when not
/// even the root of the path resolves.
fn missing_entry_location(path: &Path) -> Option<PathBuf> {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut ancestor = path;
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut location) => {
                location.extend(missing.iter().rev());
                return Some(location);
            }
            Err(_) => {
                missing.push(ancestor.file_name()?);
                ancestor = ancestor.parent()?;
            }
        }
    }
}

#[cfg(test)]
mod builtin_protected_paths_tests {
    use super::*;

    fn capture(path: &Path) -> HostFilesystemLocation {
        HostFilesystemLocation::capture(path).expect("capture existing path")
    }

    /// A fake home directory holding one protected *directory* and one
    /// protected *file*, plus the path of an entry that isn't there.
    fn fake_home() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let home = tempfile::tempdir().expect("create temp home");
        let ssh = home.path().join(".ssh");
        let bashrc = home.path().join(".bashrc");
        std::fs::create_dir(&ssh).expect("create .ssh");
        std::fs::write(&bashrc, "x").expect("write .bashrc");
        let missing = home.path().join(".aws");
        (home, ssh, bashrc, missing)
    }

    #[test]
    fn existing_entries_are_protected_and_absent_ones_are_kept_for_the_carve() {
        let (_home, ssh, bashrc, missing) = fake_home();

        let setup = merge_protected_paths(
            Vec::new(),
            &[],
            vec![ssh.clone(), bashrc.clone(), missing.clone()],
        )
        .expect("an absent built-in entry must be skipped, not fatal");

        assert!(setup.paths.contains(&capture(&ssh)), "a directory entry");
        assert!(
            setup.paths.contains(&capture(&bashrc)),
            "a plain-file entry"
        );
        assert_eq!(
            setup.paths.len(),
            if PROTECTS_NOT_YET_EXISTING_PATHS {
                3
            } else {
                2
            }
        );
        // ADR-F49 decision 8: where the backend cannot protect a path that is
        // not there, the entry is remembered so the root that would hold it is
        // carved instead — and where it can, there is nothing left to carve.
        let expected_missing: Vec<PathBuf> = if PROTECTS_NOT_YET_EXISTING_PATHS {
            Vec::new()
        } else {
            vec![
                std::fs::canonicalize(missing.parent().expect("a parent"))
                    .expect("canonical home")
                    .join(".aws"),
            ]
        };
        assert_eq!(setup.missing, expected_missing);
    }

    #[test]
    fn an_entry_the_caller_already_named_is_not_added_twice() {
        let (_home, ssh, _bashrc, _missing) = fake_home();

        let setup = merge_protected_paths(vec![capture(&ssh)], &[], vec![ssh.clone()])
            .expect("merging the caller's own entry must succeed");

        assert_eq!(
            setup.paths.len(),
            1,
            "the same object must not be pinned twice"
        );
    }

    /// A write grant *inside* a built-in entry is refused rather than silently
    /// winning or silently losing: the command would otherwise run believing it
    /// has access it does not have, or with access it must not have.
    #[test]
    fn a_writable_grant_covered_by_a_builtin_entry_is_refused() {
        let (_home, ssh, _bashrc, _missing) = fake_home();
        let inside = ssh.join("keys");
        std::fs::create_dir(&inside).expect("create .ssh/keys");

        for writable_path in [ssh.clone(), inside] {
            let error =
                merge_protected_paths(Vec::new(), &[capture(&writable_path)], vec![ssh.clone()])
                    .expect_err("a grant to a built-in protected path must be refused");
            assert!(matches!(error, SandboxError::InvalidRequest(_)));
        }
    }

    /// A **dangling symlink** is not "absent": the entry is there, and a write
    /// through it *creates* its target — `~/.bashrc -> dotfiles/bashrc` inside
    /// the worktree would be executed at the next login. So it fails closed.
    ///
    /// Linux-only: macOS resolves a missing leaf through its parent, so the
    /// capture succeeds there and the Seatbelt literal is emitted instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dangling_symlink_entry_fails_closed() {
        let home = tempfile::tempdir().expect("create temp home");
        let bashrc = home.path().join(".bashrc");
        std::os::unix::fs::symlink(home.path().join("dotfiles/bashrc"), &bashrc)
            .expect("create dangling symlink");

        let error = merge_protected_paths(Vec::new(), &[], vec![bashrc])
            .expect_err("a dangling protected entry must not be treated as absent");
        assert!(matches!(error, SandboxError::Other(_)));
    }

    /// The home directory must be discoverable, or the built-in set can't be
    /// determined and every sandboxed command fails closed.
    #[test]
    fn the_home_directory_resolves_from_the_environment() {
        assert!(protected_paths::home_dir_from_environment().is_some());
    }

    /// `Sandbox::new` — not its callers — is what applies the built-in set, and
    /// it applies it even when the caller asked for unrestricted writes.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn sandbox_new_applies_the_builtin_set_even_when_unrestricted() {
        let home = protected_paths::home_dir_from_environment()
            .expect("a machine running the sandbox has a home directory");
        let sandbox = Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Unrestricted {
                protected_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Blocked,
        })
        .expect("the built-in set must resolve");

        let protected = sandbox.protected_paths();
        for path in protected_paths::builtin_protected_paths(&home, &[]) {
            if !PROTECTS_NOT_YET_EXISTING_PATHS && !path.exists() {
                continue;
            }
            let Ok(location) = HostFilesystemLocation::capture(&path) else {
                continue;
            };
            assert!(
                protected.contains(&location),
                "{} must be protected even with unrestricted writes",
                path.display()
            );
        }
    }

    /// The agent configurations a host agent acts on are protected in every
    /// worktree root the command may write, as its `.git` is (ADR-F49).
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn agent_configurations_in_a_writable_root_are_protected() {
        let root = tempfile::tempdir().expect("create a worktree");
        std::fs::create_dir(root.path().join(".claude")).expect("create .claude");
        std::fs::write(root.path().join(".claude").join("settings.json"), "{}")
            .expect("write .claude/settings.json");
        std::fs::write(root.path().join(".mcp.json"), "{}").expect("write .mcp.json");
        std::fs::create_dir(root.path().join(".codex")).expect("create .codex");
        std::fs::write(root.path().join("notes.txt"), "x").expect("write notes.txt");

        let sandbox = Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![capture(root.path())],
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Blocked,
        })
        .expect("a worktree holding agent configurations is a valid grant");

        let protected = sandbox.protected_paths();
        // The whole `.claude` directory is protected (hooks and skills scripts
        // under it run on the host), along with the other agent configs.
        for relative in [".claude", ".mcp.json", ".codex"] {
            let path = root.path().join(relative);
            assert!(
                protected.contains(&capture(&path)),
                "{relative} must be protected"
            );
        }
        assert!(
            !protected.contains(&capture(&root.path().join("notes.txt"))),
            "an ordinary file of the worktree is not"
        );
    }
}

impl SandboxFsPolicy {
    /// Unrestricted access dominates; otherwise the writable subtrees union.
    pub fn merge(self, other: SandboxFsPolicy) -> SandboxFsPolicy {
        match (self, other) {
            (
                SandboxFsPolicy::Unrestricted {
                    protected_paths: protected_a,
                },
                SandboxFsPolicy::Unrestricted {
                    protected_paths: protected_b,
                },
            ) => SandboxFsPolicy::Unrestricted {
                protected_paths: merge_locations(protected_a, protected_b),
            },
            (
                SandboxFsPolicy::Unrestricted {
                    protected_paths: protected_a,
                },
                SandboxFsPolicy::Restricted {
                    protected_paths: protected_b,
                    ..
                },
            )
            | (
                SandboxFsPolicy::Restricted {
                    protected_paths: protected_a,
                    ..
                },
                SandboxFsPolicy::Unrestricted {
                    protected_paths: protected_b,
                },
            ) => SandboxFsPolicy::Unrestricted {
                protected_paths: merge_locations(protected_a, protected_b),
            },
            (
                SandboxFsPolicy::Restricted {
                    writable_paths: writable_a,
                    protected_paths: protected_a,
                    readable_paths: readable_a,
                },
                SandboxFsPolicy::Restricted {
                    writable_paths: writable_b,
                    protected_paths: protected_b,
                    readable_paths: readable_b,
                },
            ) => SandboxFsPolicy::Restricted {
                writable_paths: merge_locations(writable_a, writable_b),
                protected_paths: merge_locations(protected_a, protected_b),
                readable_paths: merge_locations(readable_a, readable_b),
            },
        }
    }
}

impl SandboxNetPolicy {
    /// Unrestricted access dominates and `Blocked` is the identity; otherwise the
    /// allowed domains union.
    pub fn merge(self, other: SandboxNetPolicy) -> SandboxNetPolicy {
        match (self, other) {
            (SandboxNetPolicy::Unrestricted, _) | (_, SandboxNetPolicy::Unrestricted) => {
                SandboxNetPolicy::Unrestricted
            }
            (SandboxNetPolicy::Blocked, other) | (other, SandboxNetPolicy::Blocked) => other,
            (
                SandboxNetPolicy::Restricted {
                    allowed_domains: mut a,
                },
                SandboxNetPolicy::Restricted { allowed_domains: b },
            ) => {
                for domain in b {
                    if !a.contains(&domain) {
                        a.push(domain);
                    }
                }
                SandboxNetPolicy::Restricted { allowed_domains: a }
            }
        }
    }
}

/// A command and its execution environment, before sandbox wrapping.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandAndArgs {
    /// Program to execute.
    pub program: String,
    /// Arguments passed to `program`.
    pub args: Vec<String>,
    /// Environment variables for the spawned process.
    pub env: HashMap<String, String>,
    /// Working directory for the spawned process.
    pub cwd: Option<PathBuf>,
}

/// A command transformed to run inside the platform sandbox. Plain data: spawn
/// it however you like (PTY, `std::process`, …). The resources that must
/// outlive the spawned process live in the [`Sandbox`] that produced this, not
/// here — keep the `Sandbox` alive for the command's duration.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WrappedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// ADR-F49: what a long-running interactive program — an agent CLI on a
/// terminal — needs from the sandbox beyond what a one-shot command gets.
///
/// Its standard input is live: the helper passes its own stdin (the terminal)
/// straight through, so the caller must not redirect it to `/dev/null` the way
/// it does for one-shot agent commands. The program stays in the terminal's
/// foreground process group, so Ctrl+C, Ctrl+Z and window-size changes reach
/// it directly. The helper outside the sandbox — one-shot or not — ignores
/// SIGINT / SIGQUIT / SIGTSTP, passes SIGHUP / SIGTERM on to the program,
/// exits when the program does, and first stops whatever the program left
/// running.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InteractiveSession {
    /// The IDE channel, when the program has one.
    pub ide_forward: Option<IdeForward>,
}

/// ADR-F49: the IDE channel of an [`InteractiveSession`].
///
/// The sandboxed program connects to `127.0.0.1:inner_port` on its own
/// loopback; the helper, from outside the namespaces, splices each connection
/// to the Unix socket at `rendezvous_path`, where the IDE bridge listens and
/// multiplexes the connections over its standard I/O for
/// [`spawn_bridge_mux`]. Nothing listens on the host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeForward {
    /// The port the program connects to. Any port will do: the sandbox's
    /// network namespace is its own.
    pub inner_port: u16,
    /// Absolute path, inside the WSL distro, of the bridge's socket. It has to
    /// be in a directory the sandbox protects (the Windows
    /// `ide_rendezvous_path` names one), or the helper refuses to start.
    pub rendezvous_path: String,
}

/// ADR-F49: where an [`IdeForward`]'s port is picked. The sandbox's network
/// namespace is its own and holds nothing but this channel and the egress
/// proxy, so any port in the range serves.
#[cfg(target_os = "windows")]
pub const INNER_IDE_PORTS: std::ops::Range<u16> = 20000..60000;

/// A port in [`INNER_IDE_PORTS`], for an [`IdeForward`]. Random, so that a
/// session does not keep landing on the port that a crashed session's
/// leftovers — its lock file, a rendezvous the sweep has not reached yet —
/// still name.
///
/// The one rule both launchers follow: the sandboxed Claude Code CLI (第 1 段,
/// ADR-F49 決定 3) and an external ACP agent (決定 24).
#[cfg(target_os = "windows")]
pub fn new_inner_ide_port() -> u16 {
    use rand::Rng as _;

    rand::rng().random_range(INNER_IDE_PORTS)
}

/// Errors returned by the sandbox abstraction.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SandboxError {
    /// No sandbox implementation is available for this platform.
    UnsupportedPlatform,
    /// No usable Bubblewrap executable was found.
    BwrapNotFound,
    /// The only Bubblewrap executable found is setuid-root, which is refused.
    BwrapSetuidRejected,
    /// Bubblewrap was found but failed to create the sandbox.
    SandboxProbeFailed,
    /// The sandbox bridge executable path could not be resolved.
    BridgeExecutableUnavailable(String),
    /// Windows sandboxing through WSL is unavailable.
    WslUnavailable(String),
    /// The requested sandbox policy is not supported on this platform.
    UnsupportedPolicy(String),
    /// The sandbox request is invalid (e.g. a malformed allowed-domain).
    InvalidRequest(String),
    /// An I/O error occurred (e.g. spawning the network proxy).
    Io(String),
    /// Any other sandbox setup failure.
    Other(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxError::UnsupportedPlatform => {
                write!(formatter, "sandboxing is not supported on this platform")
            }
            SandboxError::BwrapNotFound => {
                write!(formatter, "no usable `bwrap` binary was found on PATH")
            }
            SandboxError::BwrapSetuidRejected => write!(
                formatter,
                "the only available `bwrap` is setuid-root, which Zed refuses to run"
            ),
            SandboxError::SandboxProbeFailed => {
                write!(
                    formatter,
                    "`bwrap` is present but failed to create a sandbox"
                )
            }
            SandboxError::BridgeExecutableUnavailable(message) => write!(
                formatter,
                "failed to resolve sandbox bridge executable: {message}"
            ),
            SandboxError::WslUnavailable(message) => write!(formatter, "{message}"),
            SandboxError::UnsupportedPolicy(message) => write!(formatter, "{message}"),
            SandboxError::InvalidRequest(message) => write!(formatter, "{message}"),
            SandboxError::Io(message) => write!(formatter, "{message}"),
            SandboxError::Other(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Resolved filesystem setup derived from [`SandboxFsPolicy`].
struct FsSetup {
    allow_fs_write: bool,
    writable_paths: Vec<HostFilesystemLocation>,
    protected_paths: Vec<HostFilesystemLocation>,
    /// See [`ProtectedSetup::missing`]. Only Bubblewrap has to keep a missing
    /// entry from being created; the other backends protect it by path.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    missing_protected_paths: Vec<PathBuf>,
    /// ADR-F49 decision 14: what the command may read among the read-hidden
    /// entries, checked where this platform can (see [`check_readable_paths`]).
    readable_paths: Vec<HostFilesystemLocation>,
}

/// Resolved network plan derived from [`SandboxNetPolicy`]. For the restricted
/// case the allowlist is parsed up front; the proxy itself is spawned lazily on
/// the first [`Sandbox::wrap`] so its upstream-proxy chaining can read the
/// command's environment.
enum NetSetup {
    /// The command talks to the network directly. Only reachable when nothing
    /// is left to enforce: the policy was [`SandboxNetPolicy::Unrestricted`]
    /// *and* no [`NETWORK_CEILING`] is published.
    Unrestricted,
    Blocked,
    Restricted {
        /// The command's own half of the decision. `Allowlist::any()` is how an
        /// unrestricted grant under a ceiling is expressed: it waives every
        /// sandbox-side check (host patterns, IP literals, the DNS-rebinding
        /// filter) and leaves the ceiling as the only policy that can refuse.
        allowlist: Allowlist,
    },
}

/// A live sandbox: it owns the per-policy resources (the network proxy, and on
/// macOS the temporary Seatbelt policy file) and produces sandboxed command
/// invocations via [`Sandbox::wrap`] / [`Sandbox::execute`].
///
/// Keep it alive for as long as commands wrapped by it are running; dropping it
/// tears down the proxy.
pub struct Sandbox {
    fs: FsSetup,
    network: NetSetup,
    /// In-process network proxy for the restricted-network case, spawned on the
    /// first `wrap`. Dropped on a background thread (the join blocks).
    proxy: Option<ProxyHandle>,
    /// Linux only: the host endpoint that hands the in-sandbox validator the
    /// captured `O_PATH` fds over a unix socket. Runs entirely in-process (a
    /// short-lived background thread, never a separate process) and is owned by
    /// this `Sandbox` — which is created per command — so it comes up when the
    /// command is wrapped and is torn down (thread stopped, socket removed) when
    /// the command finishes. Holds the fds, keeping their inodes pinned until
    /// then. Created lazily on the wrap that first needs it (a restricted-fs run
    /// with writable binds); a `Sandbox` normally wraps a single command.
    #[cfg(target_os = "linux")]
    validation_fd_sender: Option<linux_bubblewrap::ValidationFdSender>,
    #[cfg(target_os = "macos")]
    seatbelt_config: Option<macos_seatbelt::SeatbeltConfigFile>,
    /// See [`Sandbox::set_editor_network_allowlist`].
    #[cfg(target_os = "windows")]
    editor_network_allowlist: Option<Vec<String>>,
    /// See [`Sandbox::set_interactive_session`].
    #[cfg(target_os = "windows")]
    interactive_session: Option<InteractiveSession>,
    /// See [`Sandbox::set_ide_forward`].
    #[cfg(target_os = "windows")]
    ide_forward: Option<IdeForward>,
    /// See [`Sandbox::set_wsl_linux_environment`].
    #[cfg(target_os = "windows")]
    wsl_linux_environment: bool,
    /// See [`Sandbox::set_sandbox_session_id`].
    #[cfg(target_os = "windows")]
    sandbox_session_id: Option<String>,
}

impl Sandbox {
    /// Create a sandbox for `policy`, validating it for the current platform.
    ///
    /// This does not spawn the network proxy or probe `bwrap`; the proxy is
    /// created lazily on the first [`Sandbox::wrap`] (so it can chain through an
    /// upstream proxy named in the command's environment), and `bwrap`
    /// availability is checked separately via [`Sandbox::can_create`].
    pub fn new(policy: SandboxPolicy) -> Result<Self, SandboxError> {
        let fs = match policy.fs {
            SandboxFsPolicy::Unrestricted { protected_paths } => {
                // Even "allow every write" does not hand out the account's ssh
                // keys or let the command rewrite this fork's own settings —
                // same rule the SSH-remote helper applies under
                // `--allow-fs-write` (ADR-F18).
                let protected = add_builtin_protected_paths(protected_paths, &[])?;
                FsSetup {
                    allow_fs_write: true,
                    writable_paths: Vec::new(),
                    protected_paths: protected.paths,
                    missing_protected_paths: protected.missing,
                    readable_paths: Vec::new(),
                }
            }
            SandboxFsPolicy::Restricted {
                writable_paths,
                protected_paths,
                readable_paths,
            } => {
                validate_writable_paths_do_not_overlap_protected_paths(
                    &writable_paths,
                    &protected_paths,
                )?;
                let protected = add_builtin_protected_paths(protected_paths, &writable_paths)?;
                check_readable_paths(&readable_paths)?;
                FsSetup {
                    allow_fs_write: false,
                    writable_paths,
                    protected_paths: protected.paths,
                    missing_protected_paths: protected.missing,
                    readable_paths,
                }
            }
        };

        let network = match policy.network {
            SandboxNetPolicy::Unrestricted => resolve_unrestricted_network(),
            SandboxNetPolicy::Blocked => NetSetup::Blocked,
            SandboxNetPolicy::Restricted { allowed_domains } => {
                resolve_restricted_network(&allowed_domains)?
            }
        };

        Ok(Self {
            fs,
            network,
            proxy: None,
            #[cfg(target_os = "linux")]
            validation_fd_sender: None,
            #[cfg(target_os = "macos")]
            seatbelt_config: None,
            #[cfg(target_os = "windows")]
            editor_network_allowlist: None,
            #[cfg(target_os = "windows")]
            interactive_session: None,
            #[cfg(target_os = "windows")]
            ide_forward: None,
            #[cfg(target_os = "windows")]
            wsl_linux_environment: false,
            #[cfg(target_os = "windows")]
            sandbox_session_id: None,
        })
    }
    /// Check whether the platform sandbox can be created on this host without
    /// actually building a command or spawning the proxy. On Linux this runs a
    /// brief `bwrap` probe (call it off the main thread).
    ///
    /// This answers a purely *environmental* question — is a bwrap sandbox
    /// possible here at all (a usable `bwrap`, plus the unprivileged user
    /// namespace and mount scaffolding we rely on)? It deliberately does **not**
    /// depend on any command's writable grants or working directory: the probe
    /// runs a bare, representative sandbox and runs `true` in it. (Unlike the
    /// former Landlock probe, bwrap needs no ABI-matching against the real
    /// ruleset, so there's no reason to mirror the real command's mounts.)
    pub fn can_create(policy: &SandboxPolicy) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let permissions = linux_bubblewrap::SandboxPermissions {
                network: linux_probe_network(&policy.network),
                allow_fs_write: matches!(policy.fs, SandboxFsPolicy::Unrestricted { .. }),
            };
            linux_bubblewrap::check_can_create_sandbox(permissions).map_err(map_linux_status)
        }
        // Windows: whether WSL can host the sandbox is only known from WSL, so
        // it is found out when the command is wrapped, and reported as
        // `SandboxError::WslUnavailable` with the fix.
        #[cfg(not(target_os = "linux"))]
        {
            let _ = policy;
            Ok(())
        }
    }

    /// Windows only: the entries of the editor-wide network allowlist, for the
    /// WSL helper to nest this command's network inside (ADR-F17). Elsewhere
    /// the proxy consults the live policy published through
    /// [`set_network_ceiling`]; the WSL helper's proxy runs inside WSL, where
    /// that object cannot follow, so the caller hands over the entries as they
    /// stand when the command starts. While a ceiling is published and this was
    /// never called, the command may reach loopback only.
    #[cfg(target_os = "windows")]
    pub fn set_editor_network_allowlist(&mut self, entries: Vec<String>) {
        self.editor_network_allowlist = Some(entries);
    }

    /// What [`Self::set_editor_network_allowlist`] was given, so the caller
    /// that assembles it can check its own work without WSL. What the helper
    /// is actually told is derived from this when the command is wrapped.
    #[cfg(target_os = "windows")]
    pub fn editor_network_allowlist(&self) -> Option<&[String]> {
        self.editor_network_allowlist.as_deref()
    }

    /// Windows only: wrap the command as a long-running interactive session
    /// (ADR-F49; see [`InteractiveSession`]) rather than as a one-shot
    /// command. Spawn the result on a terminal without redirecting its input.
    #[cfg(target_os = "windows")]
    pub fn set_interactive_session(&mut self, session: InteractiveSession) {
        self.interactive_session = Some(session);
    }

    /// Windows only, ADR-F49 決定 24: give this command an IDE channel
    /// ([`IdeForward`]) without making it an interactive session — an external
    /// ACP agent, whose standard I/O carries the ACP protocol rather than a
    /// terminal, reaches Zed's restricted IDE server over it.
    ///
    /// Start the bridge that listens on `forward.rendezvous_path`
    /// ([`start_ide_bridge`]) before wrapping the command: the helper refuses a
    /// rendezvous directory that is not there yet. The channel lives as long as
    /// the command — the helper's parent tears it down when the command's
    /// standard I/O closes, one-shot or not.
    ///
    /// An interactive session set through [`Self::set_interactive_session`]
    /// carries its own; this one wins if both are set.
    #[cfg(target_os = "windows")]
    pub fn set_ide_forward(&mut self, forward: IdeForward) {
        self.ide_forward = Some(forward);
    }

    /// Windows only, ADR-F49 第 2b 段: the command wrapped by this sandbox is
    /// a Linux command inside WSL whose environment this fork wrote for Linux
    /// — an agent's own settings, a `PATH` of directories in the distro, a
    /// `HOME` inside the write grant — rather than one picked up from this
    /// Windows process.
    ///
    /// Without this the environment is filtered as a Windows process
    /// environment (`PATH`, `HOME` and the rest of the Windows-valued
    /// variables are dropped, see `is_forwardable_env_var`), which is right
    /// for a terminal command taking the project's environment along and wrong
    /// for a command written for the distro.
    #[cfg(target_os = "windows")]
    pub fn set_wsl_linux_environment(&mut self) {
        self.wsl_linux_environment = true;
    }

    /// Windows only, ADR-F49 決定 18: register the sandbox this wraps a
    /// command in under `id`, so that what the command leaves running can be
    /// listed ([`windows_wsl::list_wsl_sandbox_processes`]) and stopped
    /// ([`windows_wsl::stop_wsl_sandbox_processes`]) from outside it.
    ///
    /// The helper's parent — outside every namespace it builds — writes the
    /// registration, in a directory the command cannot write. A sandbox with
    /// no id is registered nowhere and cannot be followed. The id is the
    /// client's own (see [`validate_sandbox_session_id`]), never one a command
    /// could choose.
    #[cfg(target_os = "windows")]
    pub fn set_sandbox_session_id(&mut self, id: String) {
        self.sandbox_session_id = Some(id);
    }

    /// Transform `command` into the invocation that runs it inside this sandbox.
    /// The returned [`WrappedCommand`] is plain data; keep `self` alive while it
    /// runs (it owns the proxy and any per-command policy file).
    ///
    /// **Spawn the result from a thread that outlives the command.** On Linux
    /// the sandbox is torn down with the process that started it
    /// (`bwrap --die-with-parent`, and `PR_SET_PDEATHSIG` in the remote
    /// helper), and the kernel sends that death signal when the *thread* that
    /// created the child exits, not when the whole process does. Spawning from
    /// a thread that goes away on its own — one of `smol`'s blocking-pool
    /// threads, which retire when they sit idle — kills a command that is still
    /// running. Spawn on the executor's long-lived threads, or on one of your
    /// own that lives as long as the command.
    pub async fn wrap(&mut self, command: &CommandAndArgs) -> Result<WrappedCommand, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            self.wrap_linux(command)
        }
        #[cfg(target_os = "macos")]
        {
            self.wrap_macos(command)
        }
        #[cfg(target_os = "windows")]
        {
            self.wrap_windows(command).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = command;
            Err(SandboxError::UnsupportedPlatform)
        }
    }

    /// Run `command` inside the sandbox to completion and collect its output.
    /// Convenience for non-interactive callers; interactive callers (PTYs) use
    /// [`Sandbox::wrap`].
    #[allow(
        clippy::disallowed_methods,
        reason = "this is the blocking convenience API; interactive callers use `wrap`"
    )]
    pub async fn execute(&mut self, command: &CommandAndArgs) -> Result<Output, SandboxError> {
        let wrapped = self.wrap(command).await?;
        let mut process = std::process::Command::new(&wrapped.program);
        process.args(&wrapped.args).envs(&wrapped.env);
        if let Some(cwd) = &wrapped.cwd {
            process.current_dir(cwd);
        }
        process
            .output()
            .map_err(|error| SandboxError::Io(error.to_string()))
    }

    /// Drop this sandbox on the *current* thread, tearing down the network proxy
    /// inline (its `Drop` joins a listener thread after a loopback wakeup
    /// connect).
    ///
    /// The blanket [`Drop`] impl instead offloads that join to a fresh thread,
    /// so an accidental drop on a latency-sensitive thread (e.g. the UI thread)
    /// can never block. Call this only when you are *already* on a background
    /// thread or executor and want the teardown to finish before the surrounding
    /// task completes — it avoids spawning a throwaway thread to do work the
    /// current one can do.
    pub fn drop_on_current_thread(mut self) {
        // Drop the proxy here, synchronously, so the `Drop` impl below sees
        // `None` and doesn't spawn a thread to repeat the work.
        drop(self.proxy.take());
    }

    /// Spawn the restricted-network proxy if it isn't running yet, point the
    /// command env at it, and return its `(port, host socket path)`. Returns
    /// `None` for non-restricted network policies.
    #[cfg(not(target_os = "windows"))]
    fn ensure_restricted_proxy(
        &mut self,
        env: &mut HashMap<String, String>,
    ) -> Result<Option<(u16, Option<PathBuf>)>, SandboxError> {
        let NetSetup::Restricted { allowlist } = &self.network else {
            return Ok(None);
        };

        if self.proxy.is_none() {
            let upstream = upstream_proxy_from_env(env);
            let (events_tx, events_rx) = futures::channel::mpsc::unbounded();
            let config = ProxyConfig {
                allowlist: allowlist.clone(),
                upstream,
                events: events_tx,
                policy: None,
                // The command's own allowlist is the inner box; the editor-wide
                // egress allowlist, when the app published one, is the ceiling
                // around it. For an unrestricted grant the inner box is
                // `Allowlist::any()`, so the ceiling is all that remains. Read
                // per proxy so a policy published (or withdrawn) after this
                // process started still applies.
                outer_policy: network_ceiling(),
            };
            #[cfg(target_os = "linux")]
            let handle = ProxyHandle::spawn_unix_temp(config);
            #[cfg(not(target_os = "linux"))]
            let handle = ProxyHandle::spawn(config);
            let handle = handle.map_err(|error| {
                SandboxError::Io(format!("failed to start network proxy: {error:#}"))
            })?;
            spawn_proxy_event_logger(events_rx);
            self.proxy = Some(handle);
        }

        let proxy = self
            .proxy
            .as_ref()
            .expect("proxy was just ensured to be present");
        let port = proxy.port();
        apply_proxy_env(env, port);
        Ok(Some((port, proxy.socket_path().map(PathBuf::from))))
    }

    /// Return the protected paths for the enforcement layer. Even with broad
    /// filesystem writes, protected paths remain readable but not writable.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    fn protected_paths(&self) -> Vec<HostFilesystemLocation> {
        self.fs.protected_paths.clone()
    }

    #[cfg(target_os = "linux")]
    fn wrap_linux(&mut self, command: &CommandAndArgs) -> Result<WrappedCommand, SandboxError> {
        let mut env = command.env.clone();
        let proxy = self.ensure_restricted_proxy(&mut env)?;

        let network = match &self.network {
            NetSetup::Unrestricted => linux_bubblewrap::NetworkAccess::All,
            NetSetup::Blocked => linux_bubblewrap::NetworkAccess::None,
            NetSetup::Restricted { .. } => linux_bubblewrap::NetworkAccess::LocalhostPort(
                proxy.as_ref().map(|(port, _)| *port).unwrap_or(0),
            ),
        };
        let proxy_socket_path = proxy.as_ref().and_then(|(_, socket)| socket.clone());

        let permissions = linux_bubblewrap::SandboxPermissions {
            network,
            allow_fs_write: self.fs.allow_fs_write,
        };
        let protected_paths = self.protected_paths();
        // The roots as the user approved them, for the decisions that are about
        // grants rather than mounts: what has to stay reachable among the
        // hidden entries, and which directories are covered around them.
        let roots: Vec<&Path> = self
            .fs
            .writable_paths
            .iter()
            .map(HostFilesystemLocation::linux_canonical_path)
            .collect();
        // Build the writable binds as (captured fd, bind path) pairs in lockstep.
        // The bind *path* is the verified canonical path of each location — for a
        // persisted grant, `HostFilesystemLocation::reopen` already proved this
        // path's current inode is the approved one (`readlink(fd) == canonical`)
        // and holds the fd open pinning it. The *fd* is what the in-sandbox
        // validator compares the mounted inode against, catching any swap between
        // that host-side check and the mount. The two lists stay in the same
        // order so each fd lines up with its path on the validator side.
        let (writable_owned, writable_fds): (Vec<PathBuf>, Vec<std::os::fd::OwnedFd>) =
            carve_writable_binds(&self.fs.writable_paths, &self.fs.missing_protected_paths)?
                .into_iter()
                .unzip();
        let writable: Vec<&Path> = writable_owned.iter().map(PathBuf::as_path).collect();
        let protected_owned: Vec<PathBuf> = protected_paths
            .iter()
            .map(|location| location.linux_canonical_path().to_path_buf())
            .collect();
        let protected_paths: Vec<&Path> = protected_owned.iter().map(PathBuf::as_path).collect();
        let readable: Vec<&Path> = self
            .fs
            .readable_paths
            .iter()
            .map(HostFilesystemLocation::linux_canonical_path)
            .collect();
        let read_hiding = local_read_hiding(&roots, &readable, self.fs.allow_fs_write)?;
        let read_hidden: Vec<&Path> = read_hiding.hidden.iter().map(PathBuf::as_path).collect();
        let enclosures: Vec<&Path> = read_hiding
            .enclosures
            .iter()
            .map(PathBuf::as_path)
            .collect();

        // Stand up the host endpoint that sends the captured fds to the
        // in-sandbox validator, when this run has writable binds to verify. It's
        // an in-process background thread owned by this (per-command) `Sandbox`,
        // so it lives only for the command's duration. The sender serves its
        // descriptors to exactly one client and then tears itself down, so each
        // wrap creates a fresh one (replacing any from a prior wrap); a
        // `Sandbox` normally wraps a single command.
        if !self.fs.allow_fs_write && !writable_fds.is_empty() {
            let sender =
                linux_bubblewrap::ValidationFdSender::spawn(writable_fds).map_err(|error| {
                    SandboxError::Io(format!("failed to start sandbox bind validator: {error}"))
                })?;
            self.validation_fd_sender = Some(sender);
        }
        let validation_socket =
            self.validation_fd_sender
                .as_ref()
                .map(|sender| linux_bubblewrap::ValidationSocket {
                    host_socket_path: sender.host_socket_path(),
                    sandbox_socket_path: sender.sandbox_socket_path(),
                });

        let bridge_program = std::env::current_exe()
            .map_err(|error| SandboxError::BridgeExecutableUnavailable(error.to_string()))?;
        let bridge_program = bridge_program.to_str().ok_or_else(|| {
            SandboxError::BridgeExecutableUnavailable(format!(
                "current executable path contains invalid UTF-8: {}",
                bridge_program.display()
            ))
        })?;

        let (program, args) = linux_bubblewrap::wrap_invocation(
            bridge_program,
            permissions,
            linux_bubblewrap::BwrapMounts {
                writable_directories: &writable,
                protected_paths: &protected_paths,
                read_hidden_paths: &read_hidden,
                enclosures: &enclosures,
                readable_paths: &readable,
            },
            command.cwd.as_deref(),
            &command.program,
            &command.args,
            &command_environment_pairs(&env),
            proxy_socket_path.as_deref(),
            validation_socket,
        )
        .map_err(map_anyhow_error)?;

        // The command's environment travels in the launcher's argv and reaches
        // only the command (see `linux_bubblewrap::LAUNCHER_LOADER_VARIABLES`):
        // `bwrap`, which is dynamically linked and honors `LD_PRELOAD` before it
        // has isolated anything, is started with none of it.
        Ok(WrappedCommand {
            program,
            args,
            env: linux_bubblewrap::LAUNCHER_LOADER_VARIABLES
                .iter()
                .map(|name| (name.to_string(), String::new()))
                .collect(),
            cwd: command.cwd.clone(),
        })
    }

    #[cfg(target_os = "macos")]
    fn wrap_macos(&mut self, command: &CommandAndArgs) -> Result<WrappedCommand, SandboxError> {
        let mut env = command.env.clone();
        let proxy = self.ensure_restricted_proxy(&mut env)?;

        let network = match &self.network {
            NetSetup::Unrestricted => macos_seatbelt::NetworkAccess::All,
            NetSetup::Blocked => macos_seatbelt::NetworkAccess::None,
            NetSetup::Restricted { .. } => macos_seatbelt::NetworkAccess::LocalhostPort(
                proxy.as_ref().map(|(port, _)| *port).unwrap_or(0),
            ),
        };

        let permissions = macos_seatbelt::SandboxPermissions {
            network,
            allow_fs_write: self.fs.allow_fs_write,
        };
        let protected_paths = self.protected_paths();
        // Each location's canonical path was resolved exactly once at capture
        // time; pass it straight through as the Seatbelt rule literal. The
        // profile generator must NOT re-canonicalize (that reopened the
        // verify-vs-enforce gap); see `generate_seatbelt_config`.
        let writable: Vec<&Path> = self
            .fs
            .writable_paths
            .iter()
            .map(HostFilesystemLocation::macos_canonical_path)
            .collect();
        let protected: Vec<&Path> = protected_paths
            .iter()
            .map(HostFilesystemLocation::macos_canonical_path)
            .collect();
        let readable: Vec<&Path> = self
            .fs
            .readable_paths
            .iter()
            .map(HostFilesystemLocation::macos_canonical_path)
            .collect();
        let read_hiding = local_read_hiding(&writable, &readable, self.fs.allow_fs_write)?;
        let read_hidden: Vec<&Path> = read_hiding.hidden.iter().map(PathBuf::as_path).collect();
        let enclosures: Vec<&Path> = read_hiding
            .enclosures
            .iter()
            .map(PathBuf::as_path)
            .collect();

        // The command's environment is applied by `/usr/bin/env` inside the
        // sandbox, so `sandbox-exec` itself is started with none of it.
        let (inner_program, inner_args) = command_under_env(
            &command.program,
            &command.args,
            &command_environment_pairs(&env),
        )?;
        // SSH-agent socket handling (commit signing) is deferred, so no unix
        // sockets are allowed for now.
        let (program, args, config) = macos_seatbelt::wrap_invocation(
            &inner_program,
            &inner_args,
            &writable,
            &protected,
            &[],
            &read_hidden,
            &enclosures,
            &readable,
            permissions,
        )
        .map_err(map_anyhow_error)?;
        // Keep the temporary Seatbelt policy file alive for the command's life.
        self.seatbelt_config = Some(config);

        Ok(WrappedCommand {
            program,
            args,
            env: HashMap::new(),
            cwd: command.cwd.clone(),
        })
    }

    #[cfg(target_os = "windows")]
    async fn wrap_windows(
        &mut self,
        command: &CommandAndArgs,
    ) -> Result<WrappedCommand, SandboxError> {
        let network = wsl_network(&self.network);
        let permissions = windows_wsl::SandboxPermissions {
            network_ceiling: wsl_network_ceiling(
                &network,
                network_ceiling().is_some(),
                self.editor_network_allowlist.as_deref(),
            ),
            network,
            allow_fs_write: self.fs.allow_fs_write,
            interactive: self.interactive_session.is_some(),
            ide_forward: merge_ide_forward(
                self.ide_forward.as_ref(),
                self.interactive_session.as_ref(),
            ),
            linux_environment: self.wsl_linux_environment,
            sandbox_session_id: self.sandbox_session_id.clone(),
        };
        let writable_paths = self
            .fs
            .writable_paths
            .iter()
            .map(|location| windows_wsl::WritablePath {
                requested: location.windows_requested_path().to_path_buf(),
                canonical: location.windows_canonical_path().to_path_buf(),
            })
            .collect();
        let protected_paths: Vec<PathBuf> = self
            .protected_paths()
            .iter()
            .map(|location| location.windows_requested_path().to_path_buf())
            .collect();
        // Checked by the helper inside WSL, where they are resolved and where
        // the home they have to lie in is (see `check_readable_paths`).
        let readable_paths: Vec<PathBuf> = self
            .fs
            .readable_paths
            .iter()
            .map(|location| location.windows_canonical_path().to_path_buf())
            .collect();
        let (program, args) = windows_wsl::wrap_invocation(
            command.program.clone(),
            command.args.clone(),
            writable_paths,
            protected_paths,
            readable_paths,
            permissions,
            command.cwd.clone(),
            command.env.clone(),
        )
        .await
        .map_err(map_anyhow_error)?;

        // The command's environment reaches it through the helper's `--setenv`;
        // `wsl.exe` itself is given none of it. An empty `WSLENV` also stops
        // anything this process inherited from being forwarded into WSL around
        // the helper — `LD_PRELOAD` into the shell that verifies it, say.
        Ok(WrappedCommand {
            program,
            args,
            env: HashMap::from([("WSLENV".to_string(), String::new())]),
            cwd: command.cwd.clone(),
        })
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Dropping a `ProxyHandle` joins its listener thread after a loopback
        // wakeup connect; do that off whatever (possibly UI) thread is dropping
        // the sandbox so a slow shutdown can't stall it. Callers already on a
        // background executor should prefer `drop_on_current_thread` to avoid
        // this throwaway thread.
        if let Some(proxy) = self.proxy.take() {
            std::thread::spawn(move || drop(proxy));
        }
    }
}

// ADR-F46: arguments of the helper binary the Windows backend installs inside
// WSL (the bundled Linux `remote_server`, see `run_remote_sandbox_helper_if_invoked`).
// Shared so the Windows side, which builds the `wsl.exe` invocation, and the
// Linux side, which parses it, can't drift; gated to the two platforms using them.

/// Resolves a write grant to its canonical target inside the distro.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_SANDBOX_RESOLVE_FLAG: &str = "--wsl-sandbox-resolve-path";
/// Reports whether the distro's kernel can build the sandbox at all.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_SANDBOX_PROBE_FLAG: &str = "--wsl-sandbox-probe";
/// Prefix of the single result line [`WSL_SANDBOX_PROBE_FLAG`] prints.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_PROBE_RESULT_PREFIX: &str = "zed-wsl-probe:";
/// Selects the helper's local-WSL behavior (see `linux_remote_helper`).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const WSL_HELPER_MODE_FLAG: &str = "--wsl";
/// ADR-F49: runs the same binary as the IDE bridge (see [`ide_bridge`]).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) const IDE_BRIDGE_FLAG: &str = "--ide-bridge";
/// Handle a possible re-exec of this binary as the in-sandbox launcher (bind
/// validator + restricted-network bridge) that bwrap runs before the real
/// command. Does not return if it matched.
///
/// Call this at the top of `main`, before normal argument parsing.
#[doc(hidden)]
pub fn run_sandbox_launcher_if_invoked() {
    #[cfg(target_os = "linux")]
    linux_bubblewrap::run_launcher_if_invoked();
}

// ===========================================================================
// ADR-F18: remote (SSH) Linux sandbox helper — cross-platform surface
// ===========================================================================

/// Argv marker for the remote sandbox helper's stage-1 invocation. The client
/// (any OS) builds this into the command it runs on the remote host; the remote
/// `remote_server` binary detects it at the top of `main`.
pub const REMOTE_SANDBOX_HELPER_FLAG: &str = "--remote-sandbox-helper";

/// ADR-F49 decision 18: names the sandbox the helper is about to build, so
/// that what the command leaves running can be followed from outside it (see
/// [`sandbox_processes`]). Appended by the caller that wants to follow the
/// command; a command with no id is registered nowhere.
pub const SANDBOX_SESSION_FLAG: &str = "--session";

/// Outbound-network intent for a remote sandboxed command, as the client
/// requests it. The editor-wide allowlist travels beside it as a separate
/// ceiling around this intent, on an SSH host (see
/// [`build_remote_sandbox_helper_argv`]) as in the local WSL backend (see
/// `build_wsl_sandbox_helper_argv`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteSandboxNetwork {
    /// No outbound access at all: the holder's network namespace is left empty
    /// (no slirp, no proxy).
    Blocked,
    /// Any host, still only through the enforcing proxy (so the `--outbound-addr`
    /// isolation and the `AF_UNIX` seccomp guard still apply), and only at a
    /// public address: loopback, private, link-local and shared address space —
    /// the host's own services and its LAN — stay out of reach. An editor-wide
    /// allowlist, where the client publishes one, decides about those instead —
    /// and caps this grant to the hosts it names (ADR-F17).
    AnyHost,
    /// Only these host patterns (exact host or leading-`*.` wildcard). A name
    /// that resolves into local address space is refused as well.
    Hosts(Vec<String>),
}

/// Build the argv (everything after the helper binary path) for a remote
/// sandboxed command, up to and including the trailing `--`. The caller appends
/// the inner `program` and its `args` after the returned vector.
///
/// `readable_paths` are what the command may read among the entries hidden
/// from it (ADR-F49 decision 14); the helper refuses the whole command unless
/// each lies inside an agent installation or Zed's Node.js in the remote
/// account's Zed data directory (see [`SandboxFsPolicy::Restricted`]).
///
/// ADR-F17: `network_ceiling` is the editor-wide allowlist the helper's proxy
/// nests `network` inside — see [`helper_network_ceiling`].
///
/// Pure and platform-independent (the client is often Windows), so it is unit
/// tested on every platform. Its decode half lives in the Linux-only
/// `linux_remote_helper` module and is round-trip tested there.
pub fn build_remote_sandbox_helper_argv(
    writable_paths: &[String],
    protected_paths: &[String],
    readable_paths: &[String],
    network: &RemoteSandboxNetwork,
    network_ceiling: Option<&[String]>,
    allow_fs_write: bool,
    chdir: Option<&str>,
) -> Vec<String> {
    let mut argv = remote_sandbox_helper_policy_argv(
        writable_paths,
        protected_paths,
        readable_paths,
        network,
        allow_fs_write,
        chdir,
    );
    if let Some(entries) = helper_network_ceiling(network, network_ceiling) {
        push_network_ceiling_argv(&mut argv, entries);
    }
    argv.push("--".to_string());
    argv
}

/// ADR-F17: the editor-wide allowlist the remote helper's proxy nests a
/// command's own network inside — the remote form of the ceiling the
/// in-process proxy consults on this machine, and the same one the local WSL
/// backend sends.
///
/// `entries` is `Some` exactly when the editor publishes an allowlist, so an
/// embedder or a test that has none leaves the command's own allowlist as the
/// whole policy, as it does locally; `Some(&[])` is an allowlist that permits
/// nothing but loopback, which is not the same thing and must not be dropped.
/// A blocked command gets no proxy at all and so needs no ceiling.
fn helper_network_ceiling<'a>(
    network: &RemoteSandboxNetwork,
    entries: Option<&'a [String]>,
) -> Option<&'a [String]> {
    entries.filter(|_| !matches!(network, RemoteSandboxNetwork::Blocked))
}

/// `--network-ceiling` marks the ceiling as present even with no entries, so
/// that a published-but-empty allowlist cannot be read as none.
fn push_network_ceiling_argv(argv: &mut Vec<String>, entries: &[String]) {
    argv.push("--network-ceiling".to_string());
    for entry in entries {
        argv.extend(["--ceiling-host".to_string(), entry.clone()]);
    }
}

/// ADR-F46: what the local Windows backend hands the WSL helper on top of the
/// shared policy.
#[cfg(any(target_os = "windows", all(test, target_os = "linux")))]
#[derive(Default)]
pub(crate) struct WslHelperOptions<'a> {
    /// The editor-wide network allowlist the helper's proxy nests the
    /// command's network inside; `None` when no editor-wide allowlist is in
    /// force.
    pub(crate) network_ceiling: Option<&'a [String]>,
    /// SHA-256 of the `slirp4netns` this build installed, which the helper
    /// checks before running it.
    pub(crate) slirp_sha256: Option<&'a str>,
    /// Set in the command's environment, which `wsl.exe` does not bring
    /// across from Windows.
    pub(crate) environment: &'a [(String, String)],
    /// Then removed from it.
    pub(crate) unset_environment: &'a [&'a str],
    /// ADR-F49: run as an interactive session ([`InteractiveSession`]) rather
    /// than as a one-shot command.
    pub(crate) interactive: bool,
    /// ADR-F49: the IDE channel — an interactive session's (decision 3) or a
    /// one-shot external ACP agent's (decision 24). A command has at most one,
    /// so the caller merges the two sources before getting here.
    pub(crate) ide_forward: Option<&'a IdeForward>,
    /// ADR-F49 決定 18: the name this sandbox is registered under, so that
    /// what the command leaves running can be listed and stopped from outside
    /// it ([`SANDBOX_SESSION_FLAG`]).
    pub(crate) sandbox_session_id: Option<&'a str>,
}

/// ADR-F49: a command has one IDE channel, whichever of the two ways asked for
/// it — [`Sandbox::set_ide_forward`] (decision 24, an external ACP agent) or
/// the [`InteractiveSession`]'s own (decision 3, the agent CLI on a terminal).
/// The explicit one wins.
#[cfg(target_os = "windows")]
fn merge_ide_forward(
    explicit: Option<&IdeForward>,
    interactive_session: Option<&InteractiveSession>,
) -> Option<IdeForward> {
    explicit.cloned().or_else(|| {
        interactive_session
            .as_ref()
            .and_then(|session| session.ide_forward.clone())
    })
}

/// ADR-F46: the argv for the same helper when the local Windows backend runs it
/// inside WSL, up to and including the trailing `--`: the shared policy, WSL
/// mode, and `options`. The environment is applied before the helper's own
/// `TMPDIR` and proxy variables.
#[cfg(any(target_os = "windows", all(test, target_os = "linux")))]
pub(crate) fn build_wsl_sandbox_helper_argv(
    writable_paths: &[String],
    protected_paths: &[String],
    readable_paths: &[String],
    network: &RemoteSandboxNetwork,
    allow_fs_write: bool,
    chdir: Option<&str>,
    options: &WslHelperOptions,
) -> Vec<String> {
    let mut argv = remote_sandbox_helper_policy_argv(
        writable_paths,
        protected_paths,
        readable_paths,
        network,
        allow_fs_write,
        chdir,
    );
    argv.push(WSL_HELPER_MODE_FLAG.to_string());
    if let Some(entries) = options.network_ceiling {
        push_network_ceiling_argv(&mut argv, entries);
    }
    if let Some(digest) = options.slirp_sha256 {
        argv.extend(["--slirp-sha256".to_string(), digest.to_string()]);
    }
    for (name, value) in options.environment {
        argv.extend(["--setenv".to_string(), name.clone(), value.clone()]);
    }
    for name in options.unset_environment {
        argv.extend(["--unsetenv".to_string(), name.to_string()]);
    }
    if options.interactive {
        argv.push("--interactive".to_string());
    }
    // ADR-F49 decision 24: independent of `--interactive`, which a one-shot
    // ACP agent with an IDE channel is not.
    if let Some(forward) = options.ide_forward {
        argv.extend([
            "--ide-listen".to_string(),
            forward.inner_port.to_string(),
            "--ide-rendezvous".to_string(),
            forward.rendezvous_path.clone(),
        ]);
    }
    if let Some(session_id) = options.sandbox_session_id {
        argv.extend([SANDBOX_SESSION_FLAG.to_string(), session_id.to_string()]);
    }
    argv.push("--".to_string());
    argv
}

fn remote_sandbox_helper_policy_argv(
    writable_paths: &[String],
    protected_paths: &[String],
    readable_paths: &[String],
    network: &RemoteSandboxNetwork,
    allow_fs_write: bool,
    chdir: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![REMOTE_SANDBOX_HELPER_FLAG.to_string()];
    match network {
        RemoteSandboxNetwork::Blocked => argv.extend(["--net".to_string(), "blocked".to_string()]),
        RemoteSandboxNetwork::AnyHost => argv.extend(["--net".to_string(), "any".to_string()]),
        RemoteSandboxNetwork::Hosts(hosts) => {
            argv.extend(["--net".to_string(), "hosts".to_string()]);
            for host in hosts {
                argv.extend(["--allow-host".to_string(), host.clone()]);
            }
        }
    }
    if allow_fs_write {
        argv.push("--allow-fs-write".to_string());
    }
    for path in writable_paths {
        argv.extend(["--writable".to_string(), path.clone()]);
    }
    for path in protected_paths {
        argv.extend(["--protected".to_string(), path.clone()]);
    }
    for path in readable_paths {
        argv.extend(["--readable".to_string(), path.clone()]);
    }
    if let Some(chdir) = chdir {
        argv.extend(["--chdir".to_string(), chdir.to_string()]);
    }
    argv
}

/// Handle a possible re-exec of this binary as the remote sandbox helper — on
/// an SSH host, or inside WSL for the local Windows backend, which also runs it
/// to resolve write grants and to probe the distro (ADR-F46). A no-op off Linux
/// and when the argv doesn't match. Call at the very top of the remote server's
/// `main`, before argument parsing.
#[doc(hidden)]
pub fn run_remote_sandbox_helper_if_invoked() {
    #[cfg(target_os = "linux")]
    {
        linux_remote_helper::run_if_invoked();
    }
}

/// ADR-F46: the flag after which the helper's whole argv arrives as one
/// hex-encoded argument. On an SSH host the command line is parsed by the
/// user's login shell, which may be csh, rc or anything else whose quoting the
/// client does not reproduce; hex digits and `.` mean the same to every shell,
/// so no part of the argv — the model's command and working directory included
/// — can run as shell syntax outside the sandbox.
pub const REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG: &str = "--remote-sandbox-helper-argv-hex";

/// Packs a helper argv, which starts with [`REMOTE_SANDBOX_HELPER_FLAG`], as
/// [`REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG`] and one argument: every element in
/// lowercase hex, the elements joined by `.`, so none can split into or merge
/// with another.
pub fn encode_remote_sandbox_helper_argv(argv: &[String]) -> [String; 2] {
    let encoded = argv
        .iter()
        .map(|element| {
            element
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(".");
    [REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG.to_string(), encoded]
}

/// The argv packed by [`encode_remote_sandbox_helper_argv`], or `None` when
/// `payload` is not such a packing.
pub fn decode_remote_sandbox_helper_argv(payload: &str) -> Option<Vec<String>> {
    payload
        .split('.')
        .map(|element| {
            if element.len() % 2 != 0 || !element.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            let bytes = (0..element.len())
                .step_by(2)
                .map(|index| u8::from_str_radix(element.get(index..index + 2)?, 16).ok())
                .collect::<Option<Vec<u8>>>()?;
            String::from_utf8(bytes).ok()
        })
        .collect()
}

/// Unpacks the helper's own argv (without the program name) when it arrived
/// through [`REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG`], and returns any other argv
/// unchanged. A payload that does not unpack into a helper invocation becomes
/// one the helper rejects as malformed, so it fails closed instead of falling
/// through to the server's own argument parsing.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn expand_remote_sandbox_helper_argv(
    args: Vec<std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    let packed = matches!(
        args.as_slice(),
        [flag, _] if flag == REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG
    );
    if !packed {
        return args;
    }
    match args
        .get(1)
        .and_then(|payload| payload.to_str())
        .and_then(decode_remote_sandbox_helper_argv)
    {
        Some(argv) if argv.first().map(String::as_str) == Some(REMOTE_SANDBOX_HELPER_FLAG) => {
            argv.into_iter().map(std::ffi::OsString::from).collect()
        }
        _ => vec![
            REMOTE_SANDBOX_HELPER_FLAG.into(),
            "--malformed-hex-argv".into(),
        ],
    }
}

#[cfg(test)]
mod remote_sandbox_helper_hex_argv_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn a_csh_hostile_argv_travels_as_hex_digits_only() {
        let hostile = "'\"; touch /tmp/p; echo \"`id`$(id)!\\ \n";
        let argv = vec![
            REMOTE_SANDBOX_HELPER_FLAG.to_string(),
            "--chdir".to_string(),
            hostile.to_string(),
            "--setenv".to_string(),
            "NAME".to_string(),
            String::new(),
            "--".to_string(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            hostile.to_string(),
        ];

        let [flag, payload] = encode_remote_sandbox_helper_argv(&argv);
        assert_eq!(flag, REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG);
        assert!(
            payload
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'.')),
            "{payload}"
        );
        assert_eq!(
            decode_remote_sandbox_helper_argv(&payload),
            Some(argv.clone())
        );
        assert_eq!(
            expand_remote_sandbox_helper_argv(vec![OsString::from(flag), OsString::from(payload)]),
            argv.into_iter().map(OsString::from).collect::<Vec<_>>()
        );
    }

    #[test]
    fn anything_else_is_left_alone_or_fails_closed() {
        let plain = vec![
            OsString::from(REMOTE_SANDBOX_HELPER_FLAG),
            OsString::from("--"),
            OsString::from("true"),
        ];
        assert_eq!(expand_remote_sandbox_helper_argv(plain.clone()), plain);

        let [_, not_a_helper_invocation] =
            encode_remote_sandbox_helper_argv(&["--something-else".to_string()]);
        let malformed = vec![
            OsString::from(REMOTE_SANDBOX_HELPER_FLAG),
            OsString::from("--malformed-hex-argv"),
        ];
        for payload in ["2d2", "zz", "+f2d", "ff", not_a_helper_invocation.as_str()] {
            assert_eq!(
                expand_remote_sandbox_helper_argv(vec![
                    OsString::from(REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG),
                    OsString::from(payload),
                ]),
                malformed,
                "{payload:?} must fail closed"
            );
        }
    }
}

#[cfg(test)]
mod remote_sandbox_helper_argv_tests {
    use super::*;

    #[test]
    fn blocked_network_omits_hosts() {
        let argv = build_remote_sandbox_helper_argv(
            &["/home/u/proj".to_string()],
            &["/home/u/proj/.git".to_string()],
            &[],
            &RemoteSandboxNetwork::Blocked,
            None,
            false,
            Some("/home/u/proj"),
        );
        assert_eq!(
            argv,
            vec![
                "--remote-sandbox-helper",
                "--net",
                "blocked",
                "--writable",
                "/home/u/proj",
                "--protected",
                "/home/u/proj/.git",
                "--chdir",
                "/home/u/proj",
                "--",
            ]
        );
    }

    #[test]
    fn hosts_network_lists_each_host() {
        let argv = build_remote_sandbox_helper_argv(
            &[],
            &[],
            &[],
            &RemoteSandboxNetwork::Hosts(vec!["example.com".into(), "*.rust-lang.org".into()]),
            None,
            false,
            None,
        );
        assert_eq!(
            argv,
            vec![
                "--remote-sandbox-helper",
                "--net",
                "hosts",
                "--allow-host",
                "example.com",
                "--allow-host",
                "*.rust-lang.org",
                "--",
            ]
        );
    }

    #[test]
    fn any_network_and_allow_fs_write() {
        let argv = build_remote_sandbox_helper_argv(
            &["/w".to_string()],
            &[],
            &[],
            &RemoteSandboxNetwork::AnyHost,
            None,
            true,
            None,
        );
        assert_eq!(
            argv,
            vec![
                "--remote-sandbox-helper",
                "--net",
                "any",
                "--allow-fs-write",
                "--writable",
                "/w",
                "--",
            ]
        );
    }

    /// ADR-F17: the editor-wide allowlist reaches a remote host's helper as
    /// the ceiling around the command's own hosts, exactly as it reaches the
    /// WSL helper. A caller with no allowlist to send (an embedder, a test)
    /// leaves the argv as it was, and a blocked command — which gets no proxy
    /// — needs no ceiling.
    #[test]
    fn the_editor_allowlist_travels_as_the_ceiling() {
        let argv = |network: &RemoteSandboxNetwork, ceiling: Option<&[String]>| {
            build_remote_sandbox_helper_argv(&[], &[], &[], network, ceiling, false, None)
        };
        let entries = ["api.github.com".to_string(), "*.corp.example".to_string()];
        let hosts = RemoteSandboxNetwork::Hosts(vec!["example.com".to_string()]);

        assert_eq!(
            argv(&hosts, Some(&entries)),
            [
                "--remote-sandbox-helper",
                "--net",
                "hosts",
                "--allow-host",
                "example.com",
                "--network-ceiling",
                "--ceiling-host",
                "api.github.com",
                "--ceiling-host",
                "*.corp.example",
                "--",
            ],
            "a host the editor does not allow still reaches the helper, and the ceiling \
             beside it is what refuses the connection"
        );
        assert_eq!(
            argv(&RemoteSandboxNetwork::AnyHost, Some(&entries)),
            [
                "--remote-sandbox-helper",
                "--net",
                "any",
                "--network-ceiling",
                "--ceiling-host",
                "api.github.com",
                "--ceiling-host",
                "*.corp.example",
                "--",
            ],
            "an any-host grant is capped by the editor's allowlist rather than uncapped"
        );
        assert_eq!(
            argv(&RemoteSandboxNetwork::AnyHost, Some(&[])),
            [
                "--remote-sandbox-helper",
                "--net",
                "any",
                "--network-ceiling",
                "--",
            ],
            "an allowlist that permits nothing is a ceiling, not the absence of one"
        );
        assert_eq!(
            argv(&hosts, None),
            [
                "--remote-sandbox-helper",
                "--net",
                "hosts",
                "--allow-host",
                "example.com",
                "--",
            ]
        );
        assert_eq!(
            argv(&RemoteSandboxNetwork::Blocked, Some(&entries)),
            ["--remote-sandbox-helper", "--net", "blocked", "--"]
        );
    }
}

#[cfg(test)]
mod editor_allowlist_ceiling_tests {
    use super::*;

    fn ceiling(entries: &[&str]) -> EditorAllowlistCeiling {
        let entries: Vec<String> = entries.iter().map(ToString::to_string).collect();
        EditorAllowlistCeiling::parse(&entries).expect("valid allowlist entries")
    }

    fn allows(ceiling: &EditorAllowlistCeiling, host: &str) -> bool {
        match ceiling.evaluate(host, 443) {
            PolicyDecision::Allow {
                permit_local_addresses,
            } => {
                assert!(
                    permit_local_addresses,
                    "the ceiling leaves the rebinding filter to the command's own decision"
                );
                true
            }
            PolicyDecision::Deny(_) => false,
        }
    }

    /// The editor-wide allowlist's own matching: exact hosts, `*.` for
    /// subdomains only, IP literals, canonicalized like request hosts.
    #[test]
    fn listed_hosts_pass_and_everything_else_is_refused() {
        let ceiling = ceiling(&["api.github.com", "*.corp.example", "10.0.0.5", "[fd00::5]"]);
        for host in [
            "api.github.com",
            "API.GitHub.com.",
            "a.corp.example",
            "a.b.corp.example",
            "10.0.0.5",
            "fd00::5",
            "[fd00::5]",
        ] {
            assert!(allows(&ceiling, host), "{host} is on the allowlist");
        }
        for host in [
            "github.com",
            "evil.api.github.com",
            "corp.example",
            "10.0.0.6",
            "evil.example",
            "foo.localhost",
        ] {
            assert!(!allows(&ceiling, host), "{host} is not on the allowlist");
        }
    }

    /// Loopback is exempt, as it is for the editor's own requests, so an empty
    /// allowlist leaves exactly loopback.
    #[test]
    fn an_empty_allowlist_leaves_only_loopback() {
        let ceiling = ceiling(&[]);
        for host in [
            "localhost",
            "127.0.0.1",
            "127.8.9.10",
            "[::1]",
            "::ffff:127.0.0.1",
        ] {
            assert!(allows(&ceiling, host), "{host} is loopback");
        }
        for host in ["example.com", "10.0.0.5", "localhost.example"] {
            assert!(!allows(&ceiling, host), "{host} is not loopback");
        }
    }

    /// A refusal reads exactly like the in-process ceiling's, so a command
    /// gets the same explanation on every platform.
    #[test]
    fn refusals_name_the_editor_allowlist() {
        let ceiling = ceiling(&["api.github.com"]);
        match ceiling.evaluate("crates.io", 443) {
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy { host, explanation }) => {
                assert_eq!(host, "crates.io");
                assert_eq!(
                    explanation,
                    "host 'crates.io' is not on the network allowlist"
                );
            }
            _ => panic!("expected a refusal by the editor-wide allowlist"),
        }
    }

    #[test]
    fn malformed_entries_are_refused() {
        for entry in ["foo.*.com", "*.10.0.0.5", ""] {
            assert!(
                EditorAllowlistCeiling::parse(&[entry.to_string()]).is_err(),
                "{entry:?} must be refused"
            );
        }
    }

    /// What the Windows side sends is `egress_policy`'s patterns in their
    /// display form; every kind of pattern survives that trip unchanged.
    #[test]
    fn allowlist_entries_survive_their_display_form() {
        let listed = ["api.github.com", "*.corp.example", "10.0.0.5", "[fd00::5]"];
        let (policy, errors) = http_client::EgressPolicy::from_host_strings(listed);
        assert!(errors.is_empty());
        let entries: Vec<String> = policy.patterns().iter().map(ToString::to_string).collect();
        assert_eq!(entries, listed);
        let ceiling = EditorAllowlistCeiling::parse(&entries).expect("displayed patterns parse");
        assert!(allows(&ceiling, "x.corp.example"));
        assert!(allows(&ceiling, "fd00::5"));
    }
}

// The createability probe only needs to know whether a sandbox *can* be built
// (namespaces, `bwrap` availability); it does not enforce any grant, so it runs
// with no writable binds rather than re-deriving paths from the opaque
// locations.

/// Whether a [`SandboxNetPolicy::Unrestricted`] grant still has to run through
/// the proxy, because an editor-wide ceiling is published for it to sit under.
///
/// Read from the process-wide slot rather than captured once, so a ceiling
/// installed after this process started still applies — the same reason
/// [`Sandbox::ensure_restricted_proxy`] reads it per proxy.
#[cfg(not(target_os = "windows"))]
fn unrestricted_is_capped_by_ceiling() -> bool {
    network_ceiling_published()
}

/// Turn a [`SandboxNetPolicy::Unrestricted`] grant into a plan.
///
/// "Unrestricted" scopes the *sandbox's* own restrictions, not the editor's:
/// under a published ceiling the command is proxied like any other, with an
/// allow-any base decision so the ceiling is the only thing left that can
/// refuse a host. Without a ceiling there is nothing to enforce, and standing a
/// proxy up anyway would only narrow the grant to HTTP(S) for no benefit — so
/// that case keeps the raw network it has always had.
#[cfg(not(target_os = "windows"))]
fn resolve_unrestricted_network() -> NetSetup {
    if unrestricted_is_capped_by_ceiling() {
        NetSetup::Restricted {
            allowlist: Allowlist::any(),
        }
    } else {
        NetSetup::Unrestricted
    }
}

/// On Windows every grant is proxied anyway — by the helper inside WSL, which
/// applies the editor-wide ceiling there (see [`wsl_network_ceiling`]).
#[cfg(target_os = "windows")]
fn resolve_unrestricted_network() -> NetSetup {
    NetSetup::Unrestricted
}

/// How a network plan is handed to the WSL helper, whose own egress proxy
/// enforces it (ADR-F46). Even an unrestricted grant goes through that proxy:
/// the command's network namespace has no other way out.
///
/// The editor-wide ceiling travels separately ([`wsl_network_ceiling`]), so an
/// unrestricted grant stays `AnyHost` here exactly as it stays
/// `Allowlist::any()` under the in-process proxy elsewhere.
#[cfg(target_os = "windows")]
fn wsl_network(network: &NetSetup) -> RemoteSandboxNetwork {
    match network {
        NetSetup::Blocked => RemoteSandboxNetwork::Blocked,
        NetSetup::Unrestricted => RemoteSandboxNetwork::AnyHost,
        NetSetup::Restricted { allowlist } if allowlist.allows_any() => {
            RemoteSandboxNetwork::AnyHost
        }
        NetSetup::Restricted { allowlist } => RemoteSandboxNetwork::Hosts(
            allowlist
                .patterns()
                .iter()
                .map(ToString::to_string)
                .collect(),
        ),
    }
}

/// The editor-wide allowlist the WSL helper's proxy nests `network` inside —
/// the WSL form of [`helper_network_ceiling`], keyed on whether this process
/// published a ceiling rather than on whether the caller supplied entries. A
/// caller that never supplied them gets an empty allowlist (loopback only)
/// rather than none, so a published ceiling cannot be skipped by omission.
#[cfg(target_os = "windows")]
fn wsl_network_ceiling(
    network: &RemoteSandboxNetwork,
    ceiling_published: bool,
    entries: Option<&[String]>,
) -> Option<Vec<String>> {
    let published = ceiling_published.then(|| entries.unwrap_or_default());
    helper_network_ceiling(network, published).map(<[String]>::to_vec)
}

/// The command's environment as `(name, value)` pairs in name order, for the
/// backends that hand it to the command inside the sandbox rather than to the
/// launcher. Entries `exec` could not pass on, or that `env(1)` would read as an
/// option or as its program, are left out: an empty name, one starting with
/// `-` or containing `=`, and a NUL anywhere.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn command_environment_pairs(env: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = env
        .iter()
        .filter(|(name, value)| {
            !name.is_empty()
                && !name.starts_with('-')
                && !name.contains(['=', '\0'])
                && !value.contains('\0')
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    pairs.sort();
    pairs
}

/// `program` run by `/usr/bin/env` with `environment` applied (macOS). A program
/// whose path contains `=` would be read as one more assignment, so it is
/// refused rather than run as something else.
#[cfg(any(target_os = "macos", test))]
fn command_under_env(
    program: &str,
    args: &[String],
    environment: &[(String, String)],
) -> Result<(String, Vec<String>), SandboxError> {
    if environment.is_empty() {
        return Ok((program.to_string(), args.to_vec()));
    }
    if program.contains('=') {
        return Err(SandboxError::InvalidRequest(format!(
            "cannot pass an environment to `{program}`: its path contains `=`"
        )));
    }
    let mut env_args = Vec::with_capacity(environment.len() + args.len() + 1);
    env_args.extend(
        environment
            .iter()
            .map(|(name, value)| format!("{name}={value}")),
    );
    env_args.push(program.to_string());
    env_args.extend(args.iter().cloned());
    Ok(("/usr/bin/env".to_string(), env_args))
}

#[cfg(test)]
mod command_environment_tests {
    use super::*;

    #[test]
    fn only_passable_variables_are_forwarded_in_name_order() {
        let env = HashMap::from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("LD_PRELOAD".to_string(), "/tmp/x.so".to_string()),
            (String::new(), "empty name".to_string()),
            ("-u".to_string(), "an option".to_string()),
            ("A=B".to_string(), "assignment".to_string()),
            ("NUL".to_string(), "a\0b".to_string()),
        ]);
        assert_eq!(
            command_environment_pairs(&env),
            [
                ("LD_PRELOAD".to_string(), "/tmp/x.so".to_string()),
                ("PATH".to_string(), "/usr/bin".to_string()),
            ]
        );
    }

    #[test]
    fn env_applies_the_environment_inside_the_sandbox() {
        let environment = [("GREETING".to_string(), "a b=c".to_string())];
        let (program, args) = command_under_env(
            "/bin/sh",
            &["-c".to_string(), "echo hi".to_string()],
            &environment,
        )
        .expect("an ordinary program");
        assert_eq!(program, "/usr/bin/env");
        assert_eq!(args, ["GREETING=a b=c", "/bin/sh", "-c", "echo hi"]);

        let (program, args) =
            command_under_env("/bin/sh", &["-c".to_string()], &[]).expect("no environment");
        assert_eq!(
            (program.as_str(), args.as_slice()),
            ("/bin/sh", ["-c".to_string()].as_slice())
        );

        assert!(command_under_env("/opt/a=b/tool", &[], &environment).is_err());
    }
}

fn resolve_restricted_network(allowed_domains: &[String]) -> Result<NetSetup, SandboxError> {
    let mut patterns = Vec::with_capacity(allowed_domains.len());
    for domain in allowed_domains {
        let pattern = HostPattern::parse(domain).map_err(|error| {
            SandboxError::InvalidRequest(format!("invalid network host '{domain}': {error}"))
        })?;
        patterns.push(pattern);
    }
    Ok(NetSetup::Restricted {
        allowlist: Allowlist::from_patterns(patterns),
    })
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    use super::*;

    /// ADR-F49 第 2b 段: an ext4 worktree reaches the sandbox as a
    /// `\\wsl.localhost\<distro>\…` path, which has several spellings. The
    /// overlap check has to read them as the one folder they are, or a write
    /// grant that lands on a protected path is only refused later, by the
    /// helper inside WSL, with no explanation of which paths collided.
    #[test]
    fn a_write_grant_overlapping_a_protected_path_is_seen_through_either_spelling() {
        let capture = |path: &str| HostFilesystemLocation::capture(path).expect("a grant shape");
        let protected = capture(r"\\wsl.localhost\Ubuntu\p\.git");
        for writable in [
            r"\\wsl$\Ubuntu\p\.git",
            r"\\wsl$\Ubuntu\p\.git\hooks",
            r"\\?\UNC\wsl.localhost\Ubuntu\p\.git",
            "//wsl.localhost/Ubuntu/p/.git",
        ] {
            assert!(
                writable_path_overlaps_protected_path(&capture(writable), &protected),
                "{writable} is inside the protected path"
            );
        }
        // A sibling, and the same name in another distribution, are not.
        for unrelated in [
            r"\\wsl.localhost\Ubuntu\p\src",
            r"\\wsl.localhost\Debian\p\.git",
            r"C:\p\.git",
        ] {
            assert!(
                !writable_path_overlaps_protected_path(&capture(unrelated), &protected),
                "{unrelated} is not inside the protected path"
            );
        }
    }

    /// Every grant reaches the WSL helper as a proxy policy; host lists stay
    /// host lists instead of widening to all hosts.
    #[test]
    fn network_plans_map_onto_the_wsl_helper_proxy() {
        assert_eq!(
            wsl_network(&NetSetup::Blocked),
            RemoteSandboxNetwork::Blocked
        );
        assert_eq!(
            wsl_network(&NetSetup::Unrestricted),
            RemoteSandboxNetwork::AnyHost
        );
        let restricted =
            resolve_restricted_network(&["github.com".to_string(), "*.npmjs.org".to_string()])
                .expect("valid host patterns");
        assert_eq!(
            wsl_network(&restricted),
            RemoteSandboxNetwork::Hosts(vec!["github.com".to_string(), "*.npmjs.org".to_string()])
        );
        assert_eq!(
            wsl_network(&NetSetup::Restricted {
                allowlist: Allowlist::any()
            }),
            RemoteSandboxNetwork::AnyHost
        );
    }

    /// The editor-wide allowlist reaches the helper exactly when this process
    /// published a ceiling, for host lists and unrestricted grants alike.
    #[test]
    fn editor_allowlist_reaches_the_helper_only_under_a_published_ceiling() {
        let entries = vec!["api.github.com".to_string(), "*.corp.example".to_string()];
        let any_host = RemoteSandboxNetwork::AnyHost;
        let hosts = RemoteSandboxNetwork::Hosts(vec!["github.com".to_string()]);

        assert_eq!(wsl_network_ceiling(&any_host, false, Some(&entries)), None);
        assert_eq!(
            wsl_network_ceiling(&any_host, true, Some(&entries)),
            Some(entries.clone())
        );
        assert_eq!(
            wsl_network_ceiling(&hosts, true, Some(&entries)),
            Some(entries.clone())
        );
        // Published but never supplied: loopback only, never uncapped.
        assert_eq!(wsl_network_ceiling(&any_host, true, None), Some(Vec::new()));
        assert_eq!(
            wsl_network_ceiling(&RemoteSandboxNetwork::Blocked, true, Some(&entries)),
            None
        );
    }

    #[test]
    fn wsl_helper_argv_carries_the_editor_allowlist() {
        let argv = |ceiling: Option<&[String]>| {
            build_wsl_sandbox_helper_argv(
                &[],
                &[],
                &[],
                &RemoteSandboxNetwork::AnyHost,
                false,
                None,
                &WslHelperOptions {
                    network_ceiling: ceiling,
                    ..WslHelperOptions::default()
                },
            )
        };
        let entries = ["api.github.com".to_string(), "10.0.0.5".to_string()];
        assert_eq!(
            argv(Some(&entries)),
            [
                "--remote-sandbox-helper",
                "--net",
                "any",
                "--wsl",
                "--network-ceiling",
                "--ceiling-host",
                "api.github.com",
                "--ceiling-host",
                "10.0.0.5",
                "--",
            ]
        );
        assert_eq!(
            argv(Some(&[])),
            [
                "--remote-sandbox-helper",
                "--net",
                "any",
                "--wsl",
                "--network-ceiling",
                "--",
            ]
        );
        assert_eq!(
            argv(None),
            ["--remote-sandbox-helper", "--net", "any", "--wsl", "--"]
        );
    }

    /// An interactive session is marked as one, and an IDE channel reaches the
    /// helper just before the command — with or without `--interactive`
    /// (ADR-F49 decision 24).
    #[test]
    fn wsl_helper_argv_carries_the_interactive_session_and_the_ide_channel() {
        let argv = |interactive: bool, ide_forward: Option<&IdeForward>| {
            build_wsl_sandbox_helper_argv(
                &[],
                &[],
                &[],
                &RemoteSandboxNetwork::Blocked,
                false,
                None,
                &WslHelperOptions {
                    interactive,
                    ide_forward,
                    ..WslHelperOptions::default()
                },
            )
        };
        assert_eq!(
            argv(true, None),
            [
                "--remote-sandbox-helper",
                "--net",
                "blocked",
                "--wsl",
                "--interactive",
                "--",
            ]
        );
        let forward = IdeForward {
            inner_port: 45123,
            rendezvous_path: "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock".to_string(),
        };
        assert_eq!(
            argv(true, Some(&forward)),
            [
                "--remote-sandbox-helper",
                "--net",
                "blocked",
                "--wsl",
                "--interactive",
                "--ide-listen",
                "45123",
                "--ide-rendezvous",
                "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock",
                "--",
            ]
        );
        // ADR-F49 decision 24: what `Sandbox::set_ide_forward` produces — an
        // external ACP agent is a one-shot command with an IDE channel.
        assert_eq!(
            argv(false, Some(&forward)),
            [
                "--remote-sandbox-helper",
                "--net",
                "blocked",
                "--wsl",
                "--ide-listen",
                "45123",
                "--ide-rendezvous",
                "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock",
                "--",
            ]
        );
    }

    /// ADR-F49 decision 24: a `Sandbox` reaches the helper with one IDE
    /// channel, whichever way it was asked for, and `set_ide_forward` does not
    /// make the command an interactive session.
    #[test]
    fn the_two_ways_of_asking_for_an_ide_channel_meet_in_one() {
        let forward = |port| IdeForward {
            inner_port: port,
            rendezvous_path: format!("/root/.cache/zed-jp-enhance/ide/s{port}/bridge.sock"),
        };
        assert_eq!(merge_ide_forward(None, None), None);
        assert_eq!(
            merge_ide_forward(None, Some(&InteractiveSession::default())),
            None
        );
        assert_eq!(merge_ide_forward(Some(&forward(1)), None), Some(forward(1)));
        let session = InteractiveSession {
            ide_forward: Some(forward(2)),
        };
        assert_eq!(merge_ide_forward(None, Some(&session)), Some(forward(2)));
        assert_eq!(
            merge_ide_forward(Some(&forward(1)), Some(&session)),
            Some(forward(1)),
            "the one asked for explicitly wins"
        );

        let mut sandbox = Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Blocked,
        })
        .expect("a blocked sandbox");
        sandbox.set_ide_forward(forward(45123));
        assert_eq!(sandbox.ide_forward, Some(forward(45123)));
        assert!(
            sandbox.interactive_session.is_none(),
            "an ACP agent's channel does not make it a terminal session"
        );
    }

    /// ADR-F49: both launchers pick the port of their IDE channel here, so
    /// that the range and the way it is drawn are one rule rather than two
    /// that can drift apart.
    #[test]
    fn the_inner_ide_port_is_drawn_evenly_from_one_range() {
        assert_eq!(INNER_IDE_PORTS, 20000..60000);
        const DRAWS: usize = 20_000;
        let middle = INNER_IDE_PORTS.start + (INNER_IDE_PORTS.end - INNER_IDE_PORTS.start) / 2;
        let mut upper_half = 0;
        for _ in 0..DRAWS {
            let port = new_inner_ide_port();
            assert!(
                INNER_IDE_PORTS.contains(&port),
                "{port} is outside {INNER_IDE_PORTS:?}"
            );
            if port >= middle {
                upper_half += 1;
            }
        }
        // Evenly: folding a wider number into the range with a remainder — as
        // one of the two callers this replaces did — would leave the upper
        // half of the range 39% likely instead of 50%, tens of standard
        // deviations away from this window.
        let share = upper_half as f64 / DRAWS as f64;
        assert!(
            (0.45..=0.55).contains(&share),
            "half the ports should be in the upper half of the range, not {share}"
        );
    }

    /// Restricted networking is supported on Windows now; only malformed host
    /// patterns are refused.
    #[test]
    fn host_scoped_grants_are_accepted_and_malformed_hosts_refused() {
        let sandbox = Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Restricted {
                allowed_domains: vec!["github.com".to_string()],
            },
        });
        assert!(sandbox.is_ok());
        for malformed in ["foo.*.com", "127.0.0.1"] {
            assert!(
                matches!(
                    resolve_restricted_network(&[malformed.to_string()]),
                    Err(SandboxError::InvalidRequest(_))
                ),
                "{malformed} must be refused"
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_probe_network(network: &SandboxNetPolicy) -> linux_bubblewrap::NetworkAccess {
    match network {
        // Mirror what the real run will do. Under a ceiling an unrestricted
        // grant is proxied, which needs the *unshared* network namespace —
        // strictly harder to create than the shared one `All` probes, so
        // probing `All` here could report a sandbox this host cannot build.
        SandboxNetPolicy::Unrestricted if unrestricted_is_capped_by_ceiling() => {
            linux_bubblewrap::NetworkAccess::LocalhostPort(0)
        }
        SandboxNetPolicy::Unrestricted => linux_bubblewrap::NetworkAccess::All,
        SandboxNetPolicy::Blocked => linux_bubblewrap::NetworkAccess::None,
        // The probe only needs the network namespace flag, not a real port.
        SandboxNetPolicy::Restricted { .. } => linux_bubblewrap::NetworkAccess::LocalhostPort(0),
    }
}

#[cfg(not(target_os = "windows"))]
fn upstream_proxy_from_env(env: &HashMap<String, String>) -> Option<UpstreamProxy> {
    let url = first_nonempty_env_value(
        env,
        &[
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ],
    );
    let no_proxy = first_nonempty_env_value(env, &["NO_PROXY", "no_proxy"]);
    match UpstreamProxy::parse(url, no_proxy) {
        Ok(upstream) => upstream,
        Err(error) => {
            log::warn!("[sandbox/network] ignoring upstream proxy env: {error:#}");
            None
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn first_nonempty_env_value<'a>(
    env: &'a HashMap<String, String>,
    names: &[&str],
) -> Option<&'a str> {
    for name in names {
        if let Some(value) = env.get(*name)
            && !value.trim().is_empty()
        {
            return Some(value.as_str());
        }
    }
    None
}

/// Point the child's proxy env vars at the in-process proxy and strip any
/// inherited `NO_PROXY`.
///
/// Both upper- and lower-case forms are set because some clients (notably curl
/// on macOS) only honor the lowercase variant. `NO_PROXY` is blanked so all
/// egress goes through our proxy unconditionally: an inherited `NO_PROXY`
/// matching an allowlisted host would make the client attempt a direct
/// connection, which the sandbox blocks — surfacing as a confusing "connection
/// refused" instead of a clean policy decision.
#[cfg(not(target_os = "windows"))]
fn apply_proxy_env(env: &mut HashMap<String, String>, port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        env.insert(key.to_string(), url.clone());
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.insert(key.to_string(), String::new());
    }
}

/// Drain the proxy's event channel on a background thread, logging each event.
/// v1 surfacing only; future integrations (UI, telemetry) can replace this.
/// The thread exits when the proxy is dropped and the channel closes.
#[cfg(not(target_os = "windows"))]
fn spawn_proxy_event_logger(events: futures::channel::mpsc::UnboundedReceiver<ProxyEvent>) {
    std::thread::spawn(move || {
        futures::executor::block_on(async move {
            use futures::StreamExt as _;
            let mut events = events;
            while let Some(event) = events.next().await {
                log_proxy_event(&event);
            }
        });
    });
}

#[cfg(not(target_os = "windows"))]
fn log_proxy_event(event: &ProxyEvent) {
    match event {
        ProxyEvent::Ready { .. } => {}
        ProxyEvent::RequestAttempt {
            host,
            port,
            method,
            outcome,
        } => {
            log::debug!(
                "[sandbox/network] {} {host}:{port} → {outcome:?}",
                method.as_str()
            );
        }
        ProxyEvent::RequestCompleted {
            host,
            port,
            method,
            bytes_to_remote,
            bytes_from_remote,
            duration_ms,
        } => {
            log::debug!(
                "[sandbox/network] completed {} {host}:{port} sent={bytes_to_remote} recv={bytes_from_remote} duration={duration_ms}ms",
                method.as_str(),
            );
        }
    }
}

#[cfg(target_os = "linux")]
fn map_linux_status(status: linux_bubblewrap::LauncherStatus) -> SandboxError {
    match status {
        linux_bubblewrap::LauncherStatus::BwrapNotFound => SandboxError::BwrapNotFound,
        linux_bubblewrap::LauncherStatus::SetuidRejected => SandboxError::BwrapSetuidRejected,
        linux_bubblewrap::LauncherStatus::SandboxProbeFailed => SandboxError::SandboxProbeFailed,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn map_anyhow_error(error: anyhow::Error) -> SandboxError {
    #[cfg(target_os = "windows")]
    if let Some(error) = error.downcast_ref::<windows_wsl::WslSandboxUnavailable>() {
        return SandboxError::WslUnavailable(error.to_string());
    }

    SandboxError::Other(format!("{error:#}"))
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn restricted_fs_rejects_writable_paths_inside_protected_paths() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let repo_dir = temp_dir.path().join("repo");
        let git_dir = repo_dir.join(".git");
        let hooks_dir = git_dir.join("hooks");
        std::fs::create_dir_all(&hooks_dir).expect("create git hooks dir");

        let protected_git = HostFilesystemLocation::capture(&git_dir).expect("capture git dir");
        let writable_git = HostFilesystemLocation::capture(&git_dir).expect("capture git dir");
        let writable_hooks =
            HostFilesystemLocation::capture(&hooks_dir).expect("capture hooks dir");

        for writable_path in [writable_git, writable_hooks] {
            let result = Sandbox::new(SandboxPolicy {
                fs: SandboxFsPolicy::Restricted {
                    writable_paths: vec![writable_path],
                    protected_paths: vec![protected_git.clone()],
                    readable_paths: Vec::new(),
                },
                network: SandboxNetPolicy::Blocked,
            });

            assert!(matches!(result, Err(SandboxError::InvalidRequest(_))));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn restricted_fs_protected_overlap_check_uses_captured_fds_not_path_text() {
        use std::os::unix::fs as unix_fs;

        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let repo_dir = temp_dir.path().join("repo");
        let git_dir = repo_dir.join(".git");
        let real_git_dir = repo_dir.join("real-git");
        let outside_dir = temp_dir.path().join("outside");
        let outside_hooks_dir = outside_dir.join("hooks");
        std::fs::create_dir_all(&git_dir).expect("create git dir");
        std::fs::create_dir_all(&outside_hooks_dir).expect("create outside hooks dir");

        let protected_git = HostFilesystemLocation::capture(&git_dir).expect("capture git dir");
        std::fs::rename(&git_dir, &real_git_dir).expect("move git dir aside");
        unix_fs::symlink(&outside_dir, &git_dir).expect("replace git dir with symlink");
        let writable_displayed_inside_git =
            HostFilesystemLocation::capture(git_dir.join("hooks")).expect("capture symlink target");

        Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: vec![writable_displayed_inside_git],
                protected_paths: vec![protected_git],
                readable_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Blocked,
        })
        .expect("captured writable fd is outside protected metadata");
    }

    #[test]
    fn fs_merge_unrestricted_dominates_else_unions_paths() {
        // Writable paths are captured as real `HostFilesystemLocation`s (keyed on
        // the inode), so the union/dedup test needs three distinct real dirs.
        let dir_a = tempfile::tempdir().expect("create temp dir a");
        let dir_b = tempfile::tempdir().expect("create temp dir b");
        let dir_c = tempfile::tempdir().expect("create temp dir c");
        let dir_d = tempfile::tempdir().expect("create temp dir d");
        let location = |dir: &tempfile::TempDir| {
            HostFilesystemLocation::capture(dir.path()).expect("capture temp dir")
        };

        let a = SandboxFsPolicy::Restricted {
            writable_paths: vec![location(&dir_a), location(&dir_b)],
            protected_paths: vec![location(&dir_c)],
            readable_paths: Vec::new(),
        };
        let b = SandboxFsPolicy::Restricted {
            writable_paths: vec![location(&dir_b), location(&dir_c)],
            protected_paths: vec![location(&dir_c), location(&dir_d)],
            readable_paths: Vec::new(),
        };
        assert_eq!(
            a.clone().merge(b),
            SandboxFsPolicy::Restricted {
                writable_paths: vec![location(&dir_a), location(&dir_b), location(&dir_c)],
                protected_paths: vec![location(&dir_c), location(&dir_d)],
                readable_paths: Vec::new(),
            }
        );
        assert_eq!(
            a.merge(SandboxFsPolicy::Unrestricted {
                protected_paths: vec![location(&dir_a)],
            }),
            SandboxFsPolicy::Unrestricted {
                protected_paths: vec![location(&dir_c), location(&dir_a)],
            }
        );
        assert_eq!(
            SandboxFsPolicy::Unrestricted {
                protected_paths: vec![location(&dir_d)],
            }
            .merge(SandboxFsPolicy::Restricted {
                writable_paths: vec![location(&dir_a)],
                protected_paths: vec![location(&dir_c)],
                readable_paths: Vec::new(),
            }),
            SandboxFsPolicy::Unrestricted {
                protected_paths: vec![location(&dir_d), location(&dir_c)],
            }
        );
    }

    /// ADR-F49 decision 14: what may be read unions like the writable paths,
    /// and an unrestricted policy, which has no such list, dominates.
    #[test]
    fn fs_merge_unions_readable_paths() {
        let dir_a = tempfile::tempdir().expect("create temp dir a");
        let dir_b = tempfile::tempdir().expect("create temp dir b");
        let readable = |dirs: &[&tempfile::TempDir]| SandboxFsPolicy::Restricted {
            writable_paths: Vec::new(),
            protected_paths: Vec::new(),
            readable_paths: dirs
                .iter()
                .map(|dir| HostFilesystemLocation::capture(dir.path()).expect("capture temp dir"))
                .collect(),
        };
        assert_eq!(
            readable(&[&dir_a]).merge(readable(&[&dir_b, &dir_a])),
            readable(&[&dir_a, &dir_b])
        );
        assert_eq!(
            readable(&[&dir_a]).merge(SandboxFsPolicy::Unrestricted {
                protected_paths: Vec::new(),
            }),
            SandboxFsPolicy::Unrestricted {
                protected_paths: Vec::new(),
            }
        );
    }

    #[cfg(target_os = "linux")]
    fn data_directory(home: &Path) -> PathBuf {
        home.join(".local").join("share").join("zed-jp-enhance")
    }

    #[cfg(target_os = "macos")]
    fn data_directory(home: &Path) -> PathBuf {
        home.join("Library")
            .join("Application Support")
            .join("Zed-JP-Enhance")
    }

    /// ADR-F49 decision 14: a readable location outside an agent installation
    /// or Zed's Node.js refuses the whole policy, saying why in Japanese; one
    /// inside them is accepted.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn readable_paths_outside_the_agent_runtime_refuse_the_policy() {
        let temporary = tempfile::tempdir().expect("create a home");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let data = data_directory(&home);
        let node = data.join("node");
        let database = data.join("db");
        std::fs::create_dir_all(&node).expect("create node");
        std::fs::create_dir_all(&database).expect("create db");
        let capture = |path: &Path| HostFilesystemLocation::capture(path).expect("capture");

        check_readable_paths_for(&[capture(&node)], &data, &home).expect("Node.js may be read");
        for refused in [&database, &data, &home] {
            match check_readable_paths_for(&[capture(&node), capture(refused)], &data, &home) {
                Err(SandboxError::InvalidRequest(message)) => {
                    assert!(message.contains("読み取り専用で見せる"), "{message}")
                }
                other => panic!("{} must be refused: {other:?}", refused.display()),
            }
        }

        // The account's real data directory holds no temporary directory.
        let outside = tempfile::tempdir().expect("create a directory");
        let result = Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: vec![capture(outside.path())],
            },
            network: SandboxNetPolicy::Blocked,
        });
        assert!(matches!(result, Err(SandboxError::InvalidRequest(_))));
    }

    /// ADR-F49 decisions 14 and 15 for the local backends: the agents'
    /// configuration directory and this fork's data directory are covered
    /// around what the command was granted in them, nothing on the way to what
    /// may be read is hidden, and the credentials are hidden as ever.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn local_read_hiding_covers_what_holds_the_grants_and_hides_the_rest() {
        let temporary = tempfile::tempdir().expect("create a home");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let data = data_directory(&home);
        let agents = data.join("external_agents").join("registry").join("npx");
        let agent = agents.join("claude");
        let configurations = home.join(".zed-jp-enhance-claude");
        let projects = configurations.join("projects");
        let own = projects.join("own").join("config");
        let work = home.join("work");
        for directory in [
            &agent,
            &agents.join("other"),
            &data.join("db"),
            &own,
            &projects.join("other").join("config"),
            &work,
            &home.join(".ssh"),
        ] {
            std::fs::create_dir_all(directory).expect("create");
        }

        let hiding = local_read_hiding_for_home(&home, &[&work, &own], &[&agent], false);
        assert_eq!(
            hiding.enclosures,
            vec![configurations.clone(), data.clone()]
        );
        assert!(
            hiding.hidden.contains(&home.join(".ssh")),
            "a credential store is hidden: {:?}",
            hiding.hidden
        );
        assert!(
            !hiding.hidden.iter().any(|path| agent.starts_with(path)),
            "nothing on the way to what may be read is hidden: {:?}",
            hiding.hidden
        );
        for covered in [&data, &configurations] {
            assert!(
                !hiding.hidden.iter().any(|path| path.starts_with(covered)),
                "the cover hides what is in {}: {:?}",
                covered.display(),
                hiding.hidden
            );
        }

        // A command that may write everywhere names no roots, so there is
        // nothing to bind back into a cover and the backends make none.
        let unrestricted = local_read_hiding_for_home(&home, &[], &[], true);
        assert!(unrestricted.enclosures.is_empty());
        assert!(unrestricted.hidden.contains(&data));
    }

    /// ADR-F49 decision 11: what a command may reach is used exactly as it was
    /// captured and checked. A location swapped for a symlink into a credential
    /// store after the fact takes no cover off — the entry stays hidden where
    /// it really is.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_location_that_now_leads_into_a_hidden_entry_takes_no_cover_off() {
        let temporary = tempfile::tempdir().expect("create a home");
        let home = std::fs::canonicalize(temporary.path()).expect("canonical home");
        let keys = home.join(".ssh");
        let work = home.join("work");
        std::fs::create_dir_all(&keys).expect("create .ssh");
        std::fs::create_dir(&work).expect("create a worktree");
        // As a captured location is used: its path text. What lives there now
        // is a link into the credential store.
        std::fs::remove_dir(&work).expect("remove the worktree");
        std::os::unix::fs::symlink(&keys, &work).expect("symlink");

        let hiding = local_read_hiding_for_home(&home, &[&work], &[], false);
        assert!(
            hiding.hidden.contains(&keys),
            "the credential store stays hidden: {:?}",
            hiding.hidden
        );
    }

    #[test]
    fn net_merge_unrestricted_dominates_blocked_is_identity_else_unions() {
        let hosts = |list: &[&str]| SandboxNetPolicy::Restricted {
            allowed_domains: list.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(
            hosts(&["a.com", "b.com"]).merge(hosts(&["b.com", "c.com"])),
            hosts(&["a.com", "b.com", "c.com"])
        );
        assert_eq!(
            SandboxNetPolicy::Blocked.merge(hosts(&["a.com"])),
            hosts(&["a.com"])
        );
        assert_eq!(
            hosts(&["a.com"]).merge(SandboxNetPolicy::Blocked),
            hosts(&["a.com"])
        );
        assert_eq!(
            SandboxNetPolicy::Blocked.merge(SandboxNetPolicy::Blocked),
            SandboxNetPolicy::Blocked
        );
        assert_eq!(
            hosts(&["a.com"]).merge(SandboxNetPolicy::Unrestricted),
            SandboxNetPolicy::Unrestricted
        );
    }

    /// Stand-in for the editor-wide egress ceiling. What it decides is the
    /// proxy's business (and is tested there); all these tests need is for one
    /// to exist.
    struct TestCeiling;

    impl DestinationPolicy for TestCeiling {
        fn evaluate(&self, _host: &str, _port: u16) -> http_proxy::PolicyDecision {
            http_proxy::PolicyDecision::Allow {
                permit_local_addresses: true,
            }
        }
    }

    /// The ceiling lives in a process-wide slot, so tests that install one have
    /// to take turns. A poisoned lock just means an earlier test panicked; the
    /// slot is overwritten on entry either way, so recovering is right.
    static CEILING_SLOT: RwLock<()> = RwLock::new(());

    fn with_ceiling<R>(ceiling: Option<Arc<dyn DestinationPolicy>>, body: impl FnOnce() -> R) -> R {
        let _guard = CEILING_SLOT
            .write()
            .unwrap_or_else(|error| error.into_inner());
        set_network_ceiling(ceiling);
        let result = body();
        set_network_ceiling(None);
        result
    }

    fn unrestricted_network_sandbox() -> Sandbox {
        Sandbox::new(SandboxPolicy {
            fs: SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            },
            network: SandboxNetPolicy::Unrestricted,
        })
        .expect("an unrestricted network grant is valid on every platform")
    }

    /// The ADR-F17 hole: an `allow_all_hosts` grant used to skip the proxy
    /// entirely, which meant the editor-wide allowlist never got to see the
    /// command's traffic. It has to be proxied like any other grant.
    ///
    /// The decision is made in two places — here for a command this process
    /// runs, and in the helper's own proxy for one inside WSL or on an SSH host
    /// — and under a ceiling the two have to agree, or the same grant would
    /// reach different hosts depending on where the project lives. Both leave
    /// the ceiling deciding alone, loopback and private addresses included:
    /// what makes a local model server the user listed reachable from every
    /// backend (ADR-F17, the accepted side effect of the ceiling).
    #[test]
    fn unrestricted_network_is_proxied_so_the_ceiling_still_applies() {
        with_ceiling(Some(Arc::new(TestCeiling)), || {
            let sandbox = unrestricted_network_sandbox();
            match &sandbox.network {
                // Allow-any waives the sandbox's own checks — its half of the
                // decision — and leaves the ceiling as the only one that can
                // refuse a host.
                NetSetup::Restricted { allowlist } => assert!(allowlist.allows_any()),
                NetSetup::Unrestricted | NetSetup::Blocked => {
                    panic!("an unrestricted grant under a ceiling must still be proxied")
                }
            }
        });
        #[cfg(target_os = "linux")]
        {
            let ceiling = Arc::new(
                EditorAllowlistCeiling::parse(&["example.com".to_string()])
                    .expect("valid allowlist entries"),
            );
            assert!(
                linux_remote_helper::any_host_policy(
                    &RemoteSandboxNetwork::AnyHost,
                    Some(&ceiling)
                )
                .is_none(),
                "the helper's proxy leaves a capped any-host grant to the ceiling as well"
            );
        }
    }

    /// No ceiling, nothing to enforce: proxying would only narrow the grant to
    /// HTTP(S) for no benefit, so this stays the raw network it always was.
    ///
    /// This is where the backends deliberately part company. A helper's proxy
    /// is there either way — the command's network namespace has no other way
    /// out — so an uncapped any-host grant is still judged there, and falls
    /// back to public addresses only rather than to the host's own services.
    #[test]
    fn unrestricted_network_stays_unproxied_when_no_ceiling_is_published() {
        with_ceiling(None, || {
            let sandbox = unrestricted_network_sandbox();
            assert!(matches!(&sandbox.network, NetSetup::Unrestricted));
        });
        #[cfg(target_os = "linux")]
        assert!(
            linux_remote_helper::any_host_policy(&RemoteSandboxNetwork::AnyHost, None).is_some(),
            "an uncapped any-host grant in the helper is limited to public addresses"
        );
    }

    #[test]
    fn upstream_proxy_from_env_uses_precedence_and_no_proxy() {
        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), " ".to_string());
        env.insert("https_proxy".to_string(), "http://lower:1111".to_string());
        env.insert("ALL_PROXY".to_string(), "http://all:2222".to_string());
        env.insert("HTTP_PROXY".to_string(), "http://http:3333".to_string());
        env.insert("NO_PROXY".to_string(), "".to_string());
        env.insert("no_proxy".to_string(), "internal.example".to_string());

        let upstream = upstream_proxy_from_env(&env).expect("should configure an upstream");
        assert_eq!(upstream.host, "lower");
        assert_eq!(upstream.port, 1111);
        assert!(upstream.bypasses("internal.example", 443));
        assert!(!upstream.bypasses("zed.dev", 443));
    }

    #[test]
    fn apply_proxy_env_points_vars_at_proxy_and_blanks_no_proxy() {
        let mut env = HashMap::new();
        env.insert("HTTPS_PROXY".to_string(), "http://corp:3128".to_string());
        env.insert("NO_PROXY".to_string(), "internal.example".to_string());
        env.insert("PATH".to_string(), "/usr/bin".to_string());

        apply_proxy_env(&mut env, 54321);

        for key in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            assert_eq!(
                env.get(key).map(String::as_str),
                Some("http://127.0.0.1:54321")
            );
        }
        for key in ["NO_PROXY", "no_proxy"] {
            assert_eq!(env.get(key).map(String::as_str), Some(""));
        }
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }
}

/// A directory that is about to be granted as a sandbox write path but may not
/// exist yet. Preparing one resolves the platform difference in how a
/// not-yet-existing write grant is materialized, while keeping the same security
/// property: the caller shows [`Self::canonical_path`] to the user and records
/// *that* as the grant, so approval is always against the real, symlink-resolved
/// target.
///
/// - **Linux/WSL**: bubblewrap can only bind an existing inode, so the missing
///   directory (and any missing parents) is created **eagerly**, per component,
///   and the leaf's inode is pinned to read back its canonical path. If the
///   grant is denied, [`Self::discard`] removes exactly the directories that were
///   created, deepest-first, following no symlinks (`rmdir` only removes empty
///   dirs and never traverses a swapped-in symlink).
/// - **macOS**: Seatbelt resolves paths at syscall time and can grant a missing
///   path, so nothing is created here; the directory is materialized only after
///   approval via [`Self::finalize`].
///
/// The eventual bind is still protected by the usual capture-and-revalidate path
/// (`HostFilesystemLocation`), which re-pins the inode when the command runs.
pub struct GrantableWriteDir {
    canonical_path: PathBuf,
    /// The path exactly as requested, carried so the caller can persist and
    /// display the `(raw, canonical)` pair (see [`HostFilesystemLocation`]).
    untrusted_raw_path: PathBuf,
    /// Directories created eagerly to pin the inode, shallowest-first. Empty on
    /// platforms that defer creation to [`Self::finalize`].
    eagerly_created: Vec<PathBuf>,
}

impl GrantableWriteDir {
    /// Prepare `path` for use as a sandbox write grant. `path` must be absolute.
    pub fn prepare(path: &Path) -> std::io::Result<Self> {
        let untrusted_raw_path = path.to_path_buf();
        #[cfg(target_os = "linux")]
        {
            let mut eagerly_created = Vec::new();
            if let Err(error) = create_missing_dirs(path, &mut eagerly_created) {
                // Roll back any partial creation so a failure leaves no litter.
                for dir in eagerly_created.iter().rev() {
                    let _ = std::fs::remove_dir(dir);
                }
                return Err(error);
            }
            let canonical_path = pinned_canonical_path(path)?;
            Ok(Self {
                canonical_path,
                untrusted_raw_path,
                eagerly_created,
            })
        }
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                canonical_path: canonicalize_allowing_missing_leaf(path),
                untrusted_raw_path,
                eagerly_created: Vec::new(),
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = untrusted_raw_path;
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "granting a not-yet-existing write directory is not supported on this platform",
            ))
        }
    }

    /// The canonical, symlink-resolved path to show the user and record as the
    /// grant.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// The path exactly as requested, for persisting alongside the canonical
    /// path and for the "requested → granted" approval disclosure.
    pub fn untrusted_raw_path(&self) -> &Path {
        &self.untrusted_raw_path
    }

    /// Materialize the directory once the grant is approved. A no-op on platforms
    /// that already created it eagerly.
    pub fn finalize(&self) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            std::fs::create_dir_all(&self.canonical_path)?;
        }
        Ok(())
    }

    /// Remove exactly the directories we created (deepest-first) when the grant
    /// is denied. Best-effort; `rmdir` leaves non-empty dirs and swapped-in
    /// symlinks untouched.
    pub fn discard(self) {
        for dir in self.eagerly_created.iter().rev() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Create each missing component of `path` with `create_dir` (never
/// `create_dir_all`), recording exactly the directories created so they can be
/// removed again if the grant is denied. Components that already exist are left
/// alone.
#[cfg(target_os = "linux")]
fn create_missing_dirs(path: &Path, created: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut cur = PathBuf::new();
    for component in path.components() {
        cur.push(component);
        match std::fs::create_dir(&cur) {
            Ok(()) => created.push(cur.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Open an `O_PATH` handle to `path` and read back the canonical path of the
/// inode it pins, so the value shown to the user reflects the real target even
/// when a component is a symlink.
#[cfg(target_os = "linux")]
fn pinned_canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    Ok(CanonicalPathBuf::resolve(path)?.into_path())
}

#[cfg(all(test, target_os = "linux"))]
mod grantable_write_dir_tests {
    use super::GrantableWriteDir;
    use std::fs;

    #[test]
    fn creates_missing_dirs_and_discard_removes_only_those() {
        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        fs::create_dir(&existing).unwrap();
        let target = existing.join("a").join("b").join("c");

        let prepared = GrantableWriteDir::prepare(&target).unwrap();
        assert!(target.is_dir());
        assert_eq!(prepared.canonical_path(), target.canonicalize().unwrap());

        prepared.discard();
        // Everything we created is gone...
        assert!(!existing.join("a").exists());
        // ...but the pre-existing ancestor is untouched.
        assert!(existing.is_dir());
    }

    #[test]
    fn existing_dir_is_left_alone_and_not_removed_on_discard() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("already");
        fs::create_dir(&target).unwrap();

        let prepared = GrantableWriteDir::prepare(&target).unwrap();
        assert!(target.is_dir());
        // We created nothing, so discard removes nothing.
        prepared.discard();
        assert!(target.is_dir());
    }

    #[test]
    fn canonical_path_resolves_a_symlinked_parent() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Granting `link/child` where `link` legitimately points at `real` must
        // succeed and show the user the *resolved* `real/child`, not fail.
        let prepared = GrantableWriteDir::prepare(&link.join("child")).unwrap();
        assert_eq!(
            prepared.canonical_path(),
            real.canonicalize().unwrap().join("child")
        );
        assert!(real.join("child").is_dir());

        prepared.discard();
        assert!(!real.join("child").exists());
    }
}
