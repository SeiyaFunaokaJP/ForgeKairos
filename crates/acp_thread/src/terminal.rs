use agent_client_protocol::schema::{MaybeUndefined, v1 as acp_v1, v2 as acp_v2};
use anyhow::{Result, bail};
use collections::HashMap;
use feature_flags::{FeatureFlagAppExt as _, SandboxingFeatureFlag};
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext, Context, Entity, Task};
use http_proxy::Allowlist;
use language::LanguageRegistry;
use markdown::Markdown;
use project::Project;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap as StdHashMap,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use util::{paths::PathStyle, rel_path::RelPath};

/// Request to run a terminal command inside an OS-level sandbox.
///
/// Passed to [`super::AcpThread::create_terminal`]. The actual sandboxing
/// mechanism is platform-specific (macOS Seatbelt; Linux Bubblewrap; Windows
/// via Zed's sandbox helper inside WSL), so callers describe the *intent* with plain data
/// here rather than constructing platform-specific types directly.
///
/// Default is the fully-sandboxed run (no network, project-only writes).
/// Setting `network` / `allow_fs_write` requests a relaxation; the caller is
/// responsible for having obtained user approval before reaching this point.
#[derive(Clone, Debug, Default)]
pub struct SandboxWrap {
    /// Directory subtrees the sandbox should allow writes to. Pass the
    /// project's worktree paths (and any per-command scratch directory)
    /// here — *not* the command's working directory, which is model-
    /// controlled and would let the model widen its own writable scope.
    pub writable_paths: Vec<PathBuf>,
    /// Additional write subtrees the user explicitly approved for this
    /// command (per-path write grants). Kept separate from `writable_paths`
    /// to make the trust boundary explicit: these originate from
    /// model-requested paths that passed a user-approval prompt. They are
    /// merged with `writable_paths` when generating the sandbox policy.
    ///
    /// Each grant carries the canonical target it resolved to at approval
    /// time; enforcement rebuilds the location via a verifying reopen (see
    /// [`granted_write_path_to_location`]) rather than re-resolving the bare
    /// requested path, which closes a symlink TOCTOU.
    pub extra_write_paths: Vec<settings::GrantedWritePath>,
    /// Outbound network access explicitly approved for this command.
    pub network: SandboxNetworkAccess,
    /// Additional paths that should remain readable but not writable, even when
    /// they fall under writable paths.
    pub protected_paths: Vec<PathBuf>,
    /// Allow unrestricted filesystem writes except for protected paths (ignores
    /// ordinary writable paths).
    pub allow_fs_write: bool,
    /// Whether the project (and therefore this terminal) is local. The
    /// enforcing proxy binds a loopback port on this host, so it can only
    /// confine local commands; a remote terminal can't reach it.
    pub is_local: bool,
}

#[derive(Clone, Debug, Default)]
pub enum SandboxNetworkAccess {
    /// Block all outbound network access.
    #[default]
    None,
    /// Allow only hosts in this allowlist, enforced by routing HTTP/HTTPS
    /// through an in-process proxy and confining the command to the proxy's
    /// loopback port.
    Restricted(Allowlist),
    /// Impose no host restriction of the sandbox's own. Traffic is still
    /// capped by the editor-wide egress allowlist where one is published —
    /// see [`sandbox::SandboxNetPolicy::Unrestricted`], which this becomes on
    /// this machine, and [`sandbox::RemoteSandboxNetwork::AnyHost`], which it
    /// becomes on a remote host, where the allowlist travels beside it.
    All,
}

/// A structured, serializable reason the OS sandbox could not be created for a
/// command. Mirrors the Linux/WSL Bubblewrap failure modes; surfaced to the user
/// (and persisted in tool-call metadata) so the UI can
/// explain what went wrong.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinuxWslSandboxError {
    /// No usable `bwrap` binary was found on `PATH`.
    BwrapNotFound,
    /// The only `bwrap` found is setuid-root, which Zed refuses to run.
    SetuidRejected,
    /// `bwrap` is present but couldn't set up the sandbox (typically because
    /// unprivileged user namespaces are disabled).
    SandboxProbeFailed,
    /// WSL didn't respond to any attempt at preparing the sandbox (see
    /// [`WslSandboxTimedOut`]).
    WslTimedOut { seconds: u64, attempts: usize },
    /// The WSL environment is unusable (`sandbox::SandboxError::WslUnavailable`):
    /// what to fix, in English for the agent and in the UI language for the card.
    WslUnavailable { english: String, display: String },
    /// Any other failure, with a human-readable description.
    Other(String),
}

/// Every attempt at preparing a WSL-sandboxed command ran out of time. Its own
/// error type rather than a sentence inside `sandbox::SandboxError::WslUnavailable`,
/// so that the agent is told in English while the agent panel shows it in the
/// UI language ([`LinuxWslSandboxError::WslTimedOut`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WslSandboxTimedOut {
    pub seconds: u64,
    pub attempts: usize,
}

impl std::fmt::Display for WslSandboxTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Could not create the sandbox in WSL: WSL didn't respond within {} seconds ({} \
             attempts). WSL may be slow to start. Wait a moment and try again, and if that \
             doesn't help, restart WSL with `wsl --shutdown`.",
            self.seconds, self.attempts
        )
    }
}

impl std::error::Error for WslSandboxTimedOut {}

impl From<WslSandboxTimedOut> for LinuxWslSandboxError {
    fn from(timed_out: WslSandboxTimedOut) -> Self {
        Self::WslTimedOut {
            seconds: timed_out.seconds,
            attempts: timed_out.attempts,
        }
    }
}

impl From<sandbox::SandboxError> for LinuxWslSandboxError {
    fn from(error: sandbox::SandboxError) -> Self {
        match error {
            sandbox::SandboxError::BwrapNotFound => Self::BwrapNotFound,
            sandbox::SandboxError::BwrapSetuidRejected => Self::SetuidRejected,
            sandbox::SandboxError::SandboxProbeFailed => Self::SandboxProbeFailed,
            sandbox::SandboxError::WslUnavailable(message) => Self::WslUnavailable {
                english: message.english().to_string(),
                display: message.display().to_string(),
            },
            error => Self::Other(error.to_string()),
        }
    }
}

impl LinuxWslSandboxError {
    /// A short, user-facing explanation of why the sandbox couldn't be created,
    /// suitable for display in the agent panel.
    pub fn user_facing_message(&self) -> String {
        match self {
            LinuxWslSandboxError::BwrapNotFound => {
                "No usable `bwrap` binary was found on your PATH. Install Bubblewrap to let \
                 the agent sandbox terminal commands."
                    .to_string()
            }
            LinuxWslSandboxError::SetuidRejected => {
                "The only `bwrap` available is setuid-root, which Zed refuses to run. Install \
                 a non-setuid Bubblewrap to let the agent sandbox terminal commands."
                    .to_string()
            }
            LinuxWslSandboxError::SandboxProbeFailed => {
                "`bwrap` is installed but couldn't create a sandbox, likely because \
                 unprivileged user namespaces are disabled on this system."
                    .to_string()
            }
            LinuxWslSandboxError::WslTimedOut { seconds, attempts } => WslSandboxTimedOut {
                seconds: *seconds,
                attempts: *attempts,
            }
            .to_string(),
            LinuxWslSandboxError::WslUnavailable { english, .. } => english.clone(),
            LinuxWslSandboxError::Other(message) => message.clone(),
        }
    }

    /// [`Self::user_facing_message`] in the UI language, for the agent panel.
    /// `user_facing_message` stays English because the agent is told it too,
    /// and the UI language must not change what the model reads. The probe
    /// failure also names the AppArmor profile Ubuntu needs, which only the
    /// user can install.
    pub fn display_message(&self) -> String {
        match self {
            LinuxWslSandboxError::BwrapNotFound => i18n::t!(
                "No usable `bwrap` binary was found on your PATH. Install Bubblewrap to let \
                 the agent sandbox terminal commands."
            )
            .to_string(),
            LinuxWslSandboxError::SetuidRejected => i18n::t!(
                "The only `bwrap` available is setuid-root, which Zed refuses to run. Install \
                 a non-setuid Bubblewrap to let the agent sandbox terminal commands."
            )
            .to_string(),
            LinuxWslSandboxError::SandboxProbeFailed => i18n::t!(
                "`bwrap` is installed but couldn't create a sandbox, likely because \
                 unprivileged user namespaces are disabled on this system (Ubuntu 23.10 and \
                 later need an AppArmor profile for Bubblewrap)."
            )
            .to_string(),
            LinuxWslSandboxError::WslTimedOut { seconds, attempts } => i18n::t!(
                "Could not create the sandbox in WSL: WSL didn't respond within {seconds} seconds \
                 ({attempts} attempts). WSL may be slow to start. Wait a moment and try again, \
                 and if that doesn't help, restart WSL with `wsl --shutdown`."
            ),
            LinuxWslSandboxError::WslUnavailable { display, .. } => display.clone(),
            LinuxWslSandboxError::Other(message) => message.clone(),
        }
    }

    /// The slug of the sandboxing docs section that best explains how to resolve
    /// this failure, for deep-linking from the UI. Pair with
    /// `client::zed_urls::sandboxing_docs`.
    pub fn docs_section(&self) -> &'static str {
        match self {
            // Both "no bwrap" and "only a setuid-root bwrap" are resolved by
            // installing a non-setuid Bubblewrap.
            LinuxWslSandboxError::BwrapNotFound | LinuxWslSandboxError::SetuidRejected => {
                "installing-bubblewrap"
            }
            // A failed probe on Linux is almost always disabled unprivileged
            // user namespaces, which the Ubuntu-specific section covers.
            LinuxWslSandboxError::SandboxProbeFailed => "installing-bubblewrap-ubuntu",
            LinuxWslSandboxError::WslTimedOut { .. }
            | LinuxWslSandboxError::WslUnavailable { .. } => "windows-requirements",
            // Catch-all. On Windows the requirements section says how to fix
            // each of the WSL failures.
            LinuxWslSandboxError::Other(_) => {
                if cfg!(target_os = "windows") {
                    "windows-requirements"
                } else {
                    "linux"
                }
            }
        }
    }
}

/// Rebuild a user-approved write grant into an enforceable
/// [`sandbox::HostFilesystemLocation`].
///
/// When the grant carries a resolved canonical (the normal case, established at
/// approval time), the location is rebuilt via a verifying
/// [`sandbox::HostFilesystemLocation::reopen`] — the load-bearing step of the
/// TOCTOU fix. A legacy bare-string grant (no resolved canonical) falls back to
/// a fresh [`sandbox::HostFilesystemLocation::capture`].
pub fn granted_write_path_to_location(
    granted: &settings::GrantedWritePath,
) -> std::io::Result<sandbox::HostFilesystemLocation> {
    match &granted.resolved {
        Some(resolved) => sandbox::HostFilesystemLocation::reopen(&granted.requested, resolved),
        None => sandbox::HostFilesystemLocation::capture(&granted.requested),
    }
}

/// Rebuild a grant for enforcement, or log and drop it (fail-closed) if it
/// can't be verified.
///
/// A failure here is frequently the symlink-TOCTOU defense firing: the grant's
/// canonical was redirected or replaced by a symlink since approval, so
/// [`sandbox::HostFilesystemLocation::reopen`] refuses it. That is a
/// security-relevant event, so it must be logged rather than silently
/// swallowed. The grant is dropped (the command runs without it) rather than
/// bound unverified.
///
/// Only for **display** policies (the sandbox-status UI), where a stale grant
/// should simply not be shown. Enforcement must not drop grants silently — a
/// command would run with less access than the user approved with no signal —
/// so [`SandboxWrap::to_policy`] uses the erroring
/// [`granted_write_path_to_location`] instead.
pub fn granted_write_path_to_location_or_log(
    granted: &settings::GrantedWritePath,
) -> Option<sandbox::HostFilesystemLocation> {
    granted_write_path_to_location(granted)
        .inspect_err(|error| {
            log::warn!(
                "dropping sandbox write grant {}: {error}",
                granted.requested.display()
            );
        })
        .ok()
}

impl SandboxWrap {
    /// Whether the OS sandbox for this request can actually be created right now,
    /// returning a structured [`LinuxWslSandboxError`] when it can't.
    ///
    /// The sandbox implementation never runs a command unsandboxed on its own —
    /// it aborts if it can't create the sandbox. This lets a caller decide, up
    /// front, whether to run sandboxed, fall back to an unsandboxed run
    /// (fail-open), or refuse (fail-closed). It runs a brief probe subprocess on
    /// Linux, so call it off the main thread. On platforms whose sandbox can't
    /// fail to set up this way it always returns `Ok`.
    pub fn can_create_sandbox(&self) -> Result<(), LinuxWslSandboxError> {
        let policy = self
            .to_policy()
            .map_err(|error| LinuxWslSandboxError::Other(format!("{error:#}")))?;
        sandbox::Sandbox::can_create(&policy).map_err(LinuxWslSandboxError::from)
    }

    /// Translate this request into the cross-platform [`sandbox::SandboxPolicy`].
    ///
    /// This is the enforcement-policy construction point, so it **captures** each
    /// grant as a [`sandbox::HostFilesystemLocation`] (pinning the inode / canonical
    /// path) rather than passing a re-resolvable path.
    ///
    /// This function has **no filesystem side effects**: it never creates paths,
    /// and it **fails** (rather than silently narrowing the policy) when a
    /// writable path or approved grant can't be captured — running anyway would
    /// give the command silently less access than the model and user were told
    /// it has. On Linux a writable grant that doesn't exist can't be captured
    /// (bwrap can't bind a missing path); the sanctioned way to get a grant to a
    /// new directory is the `create_directory` tool, which creates it (pinning
    /// the inode) before the grant is recorded. On macOS a missing leaf still
    /// canonicalizes, so such grants are captured directly.
    ///
    /// It is used both by the side-effect-free [`Self::can_create_sandbox`] probe
    /// and by real sandbox construction, and must behave identically.
    ///
    /// A grant failure here can also be the symlink-TOCTOU defense firing: the
    /// grant's canonical was redirected or replaced by a symlink since approval,
    /// so the verifying reopen refuses it. Failing the command surfaces that
    /// security-relevant event instead of running with the grant quietly
    /// missing.
    ///
    /// Protected paths, by contrast, are kept only where they can be captured,
    /// and the rest are dropped rather than failing the command. Unlike a
    /// writable grant, a protection can't be materialized, and failing over one
    /// would only break legitimate cases — non-git projects, single-file
    /// worktrees whose synthesized `settings.json/.git` routes through a file
    /// and can never exist. What can be captured differs by backend: Seatbelt
    /// takes a missing leaf by path, and on Windows the helper inside WSL
    /// resolves each path itself, so there a missing `.git` reaches the sandbox
    /// and its creation is denied. Bubblewrap can only pin what exists, so on
    /// Linux a missing `.git` is dropped here — and `sandbox::Sandbox::new`
    /// adds `<root>/.git` back for every writable root that has none yet, as a
    /// missing entry whose root is carved: a command can't `git init` a project
    /// without a repository and write the new `.git/config` that the editor's
    /// git and the agent's `git` tool run outside the sandbox (ADR-F49). Under
    /// `allow_fs_write` there is no root to carve, and on Linux a missing
    /// protected path stays creatable, as a missing built-in one does.
    fn to_policy(&self) -> Result<sandbox::SandboxPolicy> {
        let protected_paths = self
            .protected_paths
            .iter()
            .filter_map(|path| sandbox::HostFilesystemLocation::capture(path).ok())
            .collect::<Vec<_>>();
        let fs = if self.allow_fs_write {
            sandbox::SandboxFsPolicy::Unrestricted { protected_paths }
        } else {
            // Project worktree paths are captured fresh; user-approved grants are
            // rebuilt via the verifying reopen (or captured when legacy bare
            // strings) through `granted_write_path_to_location`. A path that
            // can't be captured fails the whole construction (never created).
            let mut locations = Vec::new();
            for path in &self.writable_paths {
                let location = sandbox::HostFilesystemLocation::capture(path).map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "cannot capture writable sandbox path `{}`",
                        path.display()
                    ))
                })?;
                locations.push(location);
            }
            for granted in &self.extra_write_paths {
                let location = granted_write_path_to_location(granted).map_err(|error| {
                    anyhow::anyhow!(error).context(format!(
                        "cannot re-verify approved sandbox write grant `{}` (if the \
                         directory was removed, remove the grant or recreate the \
                         directory)",
                        granted.requested.display()
                    ))
                })?;
                locations.push(location);
            }
            // Dedupe to a minimal cover on the captured canonical paths, so a
            // grant nested under a worktree root (or another grant) is dropped
            // rather than bound redundantly.
            let writable_paths =
                sandbox::normalize_host_filesystem_locations(locations.into_iter());
            sandbox::SandboxFsPolicy::Restricted {
                writable_paths,
                protected_paths,
                readable_paths: Vec::new(),
            }
        };
        let network = match &self.network {
            SandboxNetworkAccess::None => sandbox::SandboxNetPolicy::Blocked,
            SandboxNetworkAccess::All => sandbox::SandboxNetPolicy::Unrestricted,
            SandboxNetworkAccess::Restricted(allowlist) => sandbox::SandboxNetPolicy::Restricted {
                allowed_domains: allowlist
                    .patterns()
                    .iter()
                    .map(|pattern| pattern.to_string())
                    .collect(),
            },
        };
        Ok(sandbox::SandboxPolicy { fs, network })
    }
}

/// Why the OS sandbox was *not* applied to a terminal command, even though
/// sandboxing is active for the thread. Persisted in tool-call metadata so the
/// UI can explain the situation after the fact.
///
/// This is deliberately platform-agnostic — every variant exists on every
/// platform — so the serialized form stored in the thread database never
/// depends on which OS wrote it. Today only Linux/WSL can fail to create a
/// sandbox (`ErrorLinuxWsl`), but the variant is named so macOS/Windows can
/// grow their own failure cases later without a migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxNotAppliedReason {
    /// The user disabled the sandbox for the rest of this thread, so the command
    /// ran without one: they chose to run unsandboxed for the thread after a
    /// sandbox-creation failure (so a preceding tool call's reason is
    /// [`SandboxNotAppliedReason::ErrorLinuxWsl`]).
    DisabledForThisThread,
    /// The Linux/WSL (Bubblewrap) sandbox could not be created for this command.
    ErrorLinuxWsl(LinuxWslSandboxError),
}

/// The live sandbox kept alive for its per-command resources (the network proxy
/// and, on macOS, the Seatbelt policy file) until the terminal exits.
type SandboxConfigHandle = sandbox::Sandbox;

/// Upper bound on one attempt at preparing a WSL-sandboxed command.
/// Deliberately generous: the first invocation after the WSL utility VM has
/// shut down (or after boot) has to start the VM and the distro, which
/// routinely takes 10-30 seconds on slow disks or under antivirus scanning, and
/// the first command after installing or updating Zed also copies the sandbox
/// helper into WSL.
#[cfg(target_os = "windows")]
pub(crate) const WSL_SANDBOX_WRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How many attempts `create_terminal` makes before reporting a timeout. WSL
/// keeps booting after an attempt is abandoned, so a cold start that ran out of
/// time usually succeeds on the next one without the user noticing.
#[cfg(target_os = "windows")]
pub(crate) const WSL_SANDBOX_WRAP_ATTEMPTS: usize = 2;

/// The error reported once every attempt timed out.
#[cfg(target_os = "windows")]
pub(crate) fn wsl_sandbox_timed_out() -> WslSandboxTimedOut {
    WslSandboxTimedOut {
        seconds: WSL_SANDBOX_WRAP_TIMEOUT.as_secs(),
        attempts: WSL_SANDBOX_WRAP_ATTEMPTS,
    }
}

/// Wrap `(program, args)` for sandboxed execution, returning the wrapped
/// invocation (program, argv, env) plus the live [`sandbox::Sandbox`] that must
/// be kept alive for the command's duration. When `sandbox_wrap` is `None` the
/// command is returned unchanged.
///
/// The sandbox owns the network proxy (for restricted-network policies) and any
/// per-command policy file; the env it returns already routes through that
/// proxy when applicable.
/// ADR-F18: build the command that runs `inner_command inner_args` inside the
/// on-host remote sandbox helper (which is the remote server binary re-invoked
/// with `--remote-sandbox-helper`).
///
/// The program is the helper itself, by its `$HOME`-relative path: the remote
/// transports start a command that has no working directory in the remote home
/// (ADR-F46), so nothing — no `/bin/sh -c` to expand `$HOME` — runs in front of
/// it. `environment` reaches the command through the helper's `--setenv`,
/// which the helper applies inside its jail and before its own temp-dir and
/// proxy variables; on the helper's own process, a project-controlled
/// `LD_PRELOAD`, or a `HOME` / `PATH` the helper itself reads, would act before
/// the sandbox exists. The helper `--chdir`s into the real working directory
/// once its jail is in place.
///
/// ADR-F49: `readable_paths` are what the command runs from inside the
/// read-hidden data directory (an external agent's installation), and
/// `interactive` keeps its standard input live for a terminal session.
///
/// ADR-F49 decision 18: `session_id` names the sandbox so that what the
/// command leaves running in it can be listed from outside it later. Only an
/// agent's own process is given one; a one-off command is followed by its
/// turn, which outlives it.
///
/// ADR-F53: `ide_forward` is the IDE channel the helper opens inside the
/// command's own network namespace and splices to the bridge's rendezvous
/// socket on the host — the SSH form of what the WSL backend passes through
/// `WslHelperOptions::ide_forward`. `None` runs the command with no IDE.
///
/// ADR-F17: `network_ceiling` is the editor-wide allowlist (see
/// [`SandboxCeiling`]) that the helper's proxy nests `wrap.network`
/// inside, so that what the user granted one command can only narrow what the
/// editor as a whole may reach, never widen it. It is the client's allowlist
/// as it stands now, not the copy the remote holds, so an edit made in the IDE
/// applies to the next command without waiting for the remote to be told.
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per part of what the helper is told about the command"
)]
pub fn build_remote_sandbox_command(
    remote_binary: &RelPath,
    wrap: &SandboxWrap,
    network_ceiling: Option<&[String]>,
    cwd: Option<&Path>,
    environment: &HashMap<String, String>,
    readable_paths: &[String],
    interactive: bool,
    ide_forward: Option<&sandbox::IdeForward>,
    session_id: Option<&str>,
    inner_command: String,
    inner_args: Vec<String>,
) -> (String, Vec<String>) {
    let args = remote_sandbox_helper_argv(
        wrap,
        network_ceiling,
        cwd,
        HelperEnvironment::Argv(environment),
        readable_paths,
        interactive,
        ide_forward,
        session_id,
        inner_command,
        inner_args,
    );
    // Packed as hex because on an SSH host this command line is parsed by the
    // user's login shell, whose quoting (csh, rc, ...) the client can't match.
    (
        remote_binary.display(PathStyle::Unix).into_owned(),
        sandbox::encode_remote_sandbox_helper_argv(&args).to_vec(),
    )
}

/// [`build_remote_sandbox_command`] for an agent's own process, whose
/// environment holds its secrets: like upstream #65174 does for a remote agent
/// outside any sandbox, the environment goes to the helper on its standard
/// input (the returned bytes, to be written before the agent's own traffic)
/// instead of in its argv, which every account on the host can read.
#[allow(
    clippy::too_many_arguments,
    reason = "one argument per part of what the helper is told about the command"
)]
pub fn build_remote_sandbox_agent_command(
    remote_binary: &RelPath,
    wrap: &SandboxWrap,
    network_ceiling: Option<&[String]>,
    cwd: Option<&Path>,
    environment: &HashMap<String, String>,
    readable_paths: &[String],
    session_id: Option<&str>,
    inner_command: String,
    inner_args: Vec<String>,
) -> Result<(String, Vec<String>, Vec<u8>)> {
    let input = sandbox::encode_stdin_environment(&forwardable_environment(environment))?;
    let args = remote_sandbox_helper_argv(
        wrap,
        network_ceiling,
        cwd,
        HelperEnvironment::StandardInput,
        readable_paths,
        false,
        // ADR-F49 decision 24's IDE forward is the WSL backend's alone.
        None,
        session_id,
        inner_command,
        inner_args,
    );
    Ok((
        remote_binary.display(PathStyle::Unix).into_owned(),
        sandbox::encode_remote_sandbox_helper_argv(&args).to_vec(),
        input,
    ))
}

/// How the helper is given the command's environment.
enum HelperEnvironment<'a> {
    /// As `--setenv` arguments.
    Argv(&'a HashMap<String, String>),
    /// On its standard input ([`sandbox::SETENV_FROM_STDIN_FLAG`]).
    StandardInput,
}

#[allow(
    clippy::too_many_arguments,
    reason = "one argument per part of what the helper is told about the command"
)]
fn remote_sandbox_helper_argv(
    wrap: &SandboxWrap,
    network_ceiling: Option<&[String]>,
    cwd: Option<&Path>,
    environment: HelperEnvironment<'_>,
    readable_paths: &[String],
    interactive: bool,
    ide_forward: Option<&sandbox::IdeForward>,
    session_id: Option<&str>,
    inner_command: String,
    inner_args: Vec<String>,
) -> Vec<String> {
    let network = match &wrap.network {
        SandboxNetworkAccess::None => sandbox::RemoteSandboxNetwork::Blocked,
        SandboxNetworkAccess::All => sandbox::RemoteSandboxNetwork::AnyHost,
        SandboxNetworkAccess::Restricted(allowlist) => {
            if allowlist.allows_any() {
                sandbox::RemoteSandboxNetwork::AnyHost
            } else {
                sandbox::RemoteSandboxNetwork::Hosts(
                    allowlist
                        .patterns()
                        .iter()
                        .map(|pattern| pattern.to_string())
                        .collect(),
                )
            }
        }
    };

    let mut writable: Vec<String> = wrap
        .writable_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    for grant in &wrap.extra_write_paths {
        let path = grant.resolved.as_ref().unwrap_or(&grant.requested);
        writable.push(path.to_string_lossy().into_owned());
    }
    let protected: Vec<String> = wrap
        .protected_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    // Run the command in its working directory, or the first writable subtree.
    let chdir = cwd
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| writable.first().cloned());

    let mut args = sandbox::build_remote_sandbox_helper_argv(
        &writable,
        &protected,
        readable_paths,
        &network,
        network_ceiling,
        wrap.allow_fs_write,
        chdir.as_deref(),
    );
    // The policy ends with `--`, and the environment belongs before it.
    let separator = args.pop_if(|arg| arg == "--");
    if interactive {
        args.push("--interactive".to_string());
    }
    // ADR-F53: as in WSL, independent of `--interactive` — a one-shot ACP agent
    // with an IDE channel is not interactive.
    if let Some(forward) = ide_forward {
        args.extend(sandbox::ide_forward_argv(forward));
    }
    if let Some(session_id) = session_id {
        args.extend([
            sandbox::SANDBOX_SESSION_FLAG.to_string(),
            session_id.to_string(),
        ]);
    }
    match environment {
        HelperEnvironment::Argv(environment) => {
            for (name, value) in forwardable_environment(environment) {
                args.extend(["--setenv".to_string(), name, value]);
            }
        }
        HelperEnvironment::StandardInput => {
            args.push(sandbox::SETENV_FROM_STDIN_FLAG.to_string());
        }
    }
    args.extend(separator);
    args.push(inner_command);
    args.extend(inner_args);
    args
}

/// The variables of `environment` the helper can be given, in a stable order.
fn forwardable_environment(environment: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut environment = environment
        .iter()
        .filter(|(name, value)| can_forward_environment_variable(name, value))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Vec<_>>();
    environment.sort();
    environment
}

/// Whether a variable may be forwarded into a sandbox at all: its name a
/// portable shell name (`[A-Za-z_][A-Za-z0-9_]*`), so that it can be neither
/// an option to `env` nor refused by `setenv(3)` — the helper rejects a whole
/// invocation over one bad name — and its value one an argv can carry.
fn can_forward_environment_variable(name: &str, value: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        && !value.contains('\0')
}

/// How [`super::AcpThread::create_terminal`] runs a command (ADR-F46).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalSpawnRoute {
    /// Wrapped by the on-host helper of a remote project.
    RemoteHelper,
    /// Wrapped by this machine's sandbox.
    LocalSandbox,
    /// Not wrapped (the user confirmed it): an ordinary task terminal.
    Unwrapped,
}

/// Decides [`TerminalSpawnRoute`] once, from the wrap the caller asked for and
/// whether the project's remote still has a helper. A wrap meant for a remote
/// that no longer has one — its connection changed in between — is an error:
/// running it anyway would put a remote command into this machine's sandbox.
pub(crate) fn terminal_spawn_route(
    sandbox_wrap: Option<&SandboxWrap>,
    remote_helper_available: bool,
) -> Result<TerminalSpawnRoute> {
    match sandbox_wrap {
        None => Ok(TerminalSpawnRoute::Unwrapped),
        Some(_) if remote_helper_available => Ok(TerminalSpawnRoute::RemoteHelper),
        Some(wrap) if wrap.is_local => Ok(TerminalSpawnRoute::LocalSandbox),
        Some(_) => Err(anyhow::anyhow!(
            "The agent's command wasn't run because the remote sandbox is no longer available \
             (the connection may have changed). Reconnect and try again."
        )),
    }
}

/// Variables a local sandbox sets for the command itself — its proxy and its
/// temp directory — which [`prefix_environment`] must not override.
const SANDBOX_OWNED_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
    "TMPDIR",
    "TMP",
    "TEMP",
];

/// What a sandboxed command still gets from Zed's own environment, because a
/// command without it cannot run or address the terminal: where to find
/// programs, who the user is and where their home is, and what the terminal
/// understands. Every other variable Zed was started with — the login shell's
/// `SSH_AUTH_SOCK`, the API tokens of whatever started Zed — stays outside.
// On Windows the local sandbox is `wsl.exe`, which lets no variable of its own
// process cross into Linux unless `WSLENV` names it, so nothing there starts a
// command this way.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) const HOST_ENVIRONMENT_INSIDE_SANDBOX: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TERMINFO",
    "COLORTERM",
    "TZ",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
];

/// ADR-F49 / ADR-F46: starts `(program, args)` from an empty environment with
/// nothing but `environment` and [`HOST_ENVIRONMENT_INSIDE_SANDBOX`] on it.
///
/// A terminal starts its command through a PTY, which hands the child
/// everything the editor's own process has — `alacritty_terminal` sets the
/// variables it is given without clearing the rest — so a command Zed wrapped
/// in a sandbox would otherwise be handed the environment Zed itself was
/// started with (a login shell's agent socket, the tokens in the shell that
/// launched it), whatever the sandbox does about the filesystem and the
/// network. Clearing it here covers every backend: what the wrapper process
/// starts with is all its child can inherit.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) fn start_from_an_empty_environment(
    program: String,
    args: Vec<String>,
    environment: &HashMap<String, String>,
) -> (String, Vec<String>) {
    let mut assignments = HOST_ENVIRONMENT_INSIDE_SANDBOX
        .iter()
        .filter(|name| !environment.contains_key(**name))
        .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
        .chain(
            environment
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        )
        .filter(|(name, value)| can_forward_environment_variable(name, value))
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    assignments.sort();
    // `-i` first, and an absolute path, because this is the one command the
    // terminal starts with Zed's own `PATH` still in effect.
    let mut cleared = vec!["-i".to_string()];
    cleared.append(&mut assignments);
    cleared.push(program);
    cleared.extend(args);
    ("/usr/bin/env".to_string(), cleared)
}

/// ADR-F46: runs `(program, args)` under `env` with `environment` laid over the
/// command's own, so that a locally sandboxed command still gets `terminal.env`
/// from settings — inside the sandbox, where `env` runs — instead of on the
/// sandbox's own process.
pub(crate) fn prefix_environment(
    program: String,
    args: Vec<String>,
    environment: &HashMap<String, String>,
) -> (String, Vec<String>) {
    let mut assignments = environment
        .iter()
        .filter(|(name, value)| {
            can_forward_environment_variable(name, value)
                && !SANDBOX_OWNED_VARIABLES.contains(&name.as_str())
        })
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    if assignments.is_empty() {
        return (program, args);
    }
    assignments.sort();
    assignments.push(program);
    assignments.extend(args);
    ("env".to_string(), assignments)
}

/// ADR-F18: the exit code the on-host remote sandbox helper uses to signal that
/// it refused to run the command because it could not build the isolation
/// (fail-closed). Mirrors `SANDBOX_SETUP_FAILED_EXIT_CODE` in
/// `sandbox::linux_remote_helper`.
pub const REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE: u32 = 126;

/// The stderr marker the helper prints before the structured `<code>: <detail>`
/// failure line on an SSH host (see `sandbox::linux_remote_helper::fail_closed`).
/// The client keys the fail-closed detection on this exact prefix so an
/// ordinary command's output can never masquerade as a sandbox-creation failure.
const REMOTE_SANDBOX_FAILURE_MARKER: &str = "zed-remote-sandbox: ";

/// ADR-F46: the same helper's marker when the Windows backend runs it inside
/// WSL. The codes are shared; what the user has to fix is not.
const WSL_SANDBOX_FAILURE_MARKER: &str = "zed-wsl-sandbox: ";

/// Lines the helper prints ahead of the command's own output, and of a
/// fail-closed marker, about protection it could not apply to a command it
/// still runs.
const SANDBOX_WARNING_PREFIXES: [&str; 2] =
    ["zed-remote-sandbox-warning:", "zed-wsl-sandbox-warning:"];

/// How many warning lines are skipped looking for the marker. The helper
/// prints at most one today; the bound keeps the search at the top of the
/// output, where only the helper writes.
const MAX_LEADING_SANDBOX_WARNINGS: usize = 4;

/// ADR-F49: the warning an SSH host's helper prints when the host refused the
/// mounts that hide the credential stores. The command runs without them.
const READ_HIDING_UNAVAILABLE_WARNING: &str = "read-hiding-unavailable";

/// What Zed adds right after that warning, for the user.
const READ_HIDING_UNAVAILABLE_NOTE: &str = "On this host, commands in the sandbox can read \
     credentials (such as `~/.ssh`), because mounts inside a user namespace were refused. Write \
     protection and network isolation are still in effect.";

/// ADR-F49 decision 11: the warning for the same host when Landlock hid the
/// credential stores instead of the refused mounts. The protection is in
/// place; what it costs the command is not obvious, so it is said too.
const READ_HIDING_BY_LANDLOCK_WARNING: &str = "read-hiding-by-landlock";

/// What Zed adds right after that warning, for the user.
const READ_HIDING_BY_LANDLOCK_NOTE: &str = "This host refuses mounts inside a user namespace, so \
     Landlock denies reading credentials (such as `~/.ssh`) instead; the protection is in \
     effect. As a result, commands in the sandbox can't list the home directory, `~/.config`, `/` \
     and the like (`ls ~`), and can't read entries created directly under them after the command \
     started.";

/// The helper's read-hiding warnings and the note Zed puts after each.
const READ_HIDING_WARNINGS: [(&str, &str); 2] = [
    (
        READ_HIDING_UNAVAILABLE_WARNING,
        READ_HIDING_UNAVAILABLE_NOTE,
    ),
    (
        READ_HIDING_BY_LANDLOCK_WARNING,
        READ_HIDING_BY_LANDLOCK_NOTE,
    ),
];

/// Shared lead-in for every remote sandbox-creation failure (GLOSSARY §5).
const REMOTE_SANDBOX_FAILURE_PREFIX: &str =
    i18n::msg!("Couldn't create the sandbox on the remote host: ");

const WSL_SANDBOX_FAILURE_PREFIX: &str = i18n::msg!("Could not create the sandbox in WSL: ");

/// One-line, user-facing hint appended when a remote sandbox-creation failure is
/// surfaced: what the host needs for the sandbox helper (ADR-F18). There is no
/// way to run the command outside the sandbox instead (ADR-F46). The name is
/// kept because `terminal_tool` recognizes an already-annotated output by it.
pub const REMOTE_SANDBOX_UNSANDBOX_HINT: &str = "The remote host needs user namespaces (`unshare -Un true` has to succeed), Landlock \
     (kernel 5.13 or later) or mounts inside a user namespace, and \
     `~/.zed_server/slirp4netns` (put there automatically when connecting).";

/// Appended to a WSL failure that updating WSL's kernel fixes.
const WSL_SANDBOX_UPDATE_HINT: &str =
    i18n::msg!("Update WSL with `wsl --update`, restart it with `wsl --shutdown`, and try again.");

/// Who reads a sandbox failure message. The agent reads the terminal output and
/// the tool errors, which stay English whatever the UI language, so that the
/// UI language does not change what the model reads; the user reads the
/// launcher's error in the UI language.
#[derive(Clone, Copy)]
enum Reader {
    Agent,
    User,
}

impl Reader {
    fn text(self, message: &'static str) -> &'static str {
        match self {
            Reader::Agent => message,
            Reader::User => i18n::tr(message),
        }
    }
}

/// Where a fail-closed helper ran, which decides how its failure is worded.
#[derive(Clone, Copy)]
enum SandboxHost {
    Remote,
    Wsl,
}

impl SandboxHost {
    fn marker(self) -> &'static str {
        match self {
            SandboxHost::Remote => REMOTE_SANDBOX_FAILURE_MARKER,
            SandboxHost::Wsl => WSL_SANDBOX_FAILURE_MARKER,
        }
    }

    fn prefix(self, reader: Reader) -> &'static str {
        reader.text(match self {
            SandboxHost::Remote => REMOTE_SANDBOX_FAILURE_PREFIX,
            SandboxHost::Wsl => WSL_SANDBOX_FAILURE_PREFIX,
        })
    }

    /// The line added after the reason, if any.
    fn hint(self, code: &str) -> Option<&'static str> {
        match self {
            SandboxHost::Remote => Some(REMOTE_SANDBOX_UNSANDBOX_HINT),
            SandboxHost::Wsl => matches!(
                code,
                "userns-failed"
                    | "no-fs-backend"
                    | "landlock-failed"
                    | "mount-failed"
                    | "seccomp-failed"
                    | "caps-failed"
                    | "pidns-failed"
            )
            .then_some(WSL_SANDBOX_UPDATE_HINT),
        }
    }
}

/// ADR-F18: recognize the fail-closed signature the remote sandbox helper emits
/// — exit code 126 *and* a `zed-remote-sandbox: <code>: <detail>` marker line at
/// the start of stderr — and return the explanation for `<code>` (GLOSSARY §5),
/// in English: the agent reads it. [`remote_sandbox_setup_failure_display_message`]
/// is the same in the UI language.
///
/// Returns `None` for anything else, so an ordinary non-zero command exit is
/// never mistaken for a sandbox-creation failure: **both** the exact exit code
/// and the marker prefix must be present. `output` is the terminal's combined
/// stdout/stderr; only the marker line, the first one after the helper's
/// warnings, is parsed. The command never ran when this fires, so the caller
/// should present the reason rather than treat the exit code as the command's
/// own result.
pub fn remote_sandbox_setup_failure_message(
    exit_code: Option<u32>,
    output: &str,
) -> Option<String> {
    if exit_code != Some(REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE) {
        return None;
    }
    let code = sandbox_failure_code(output, SandboxHost::Remote)?;
    Some(sandbox_failure_message_for_code(
        SandboxHost::Remote,
        code,
        Reader::Agent,
    ))
}

/// [`remote_sandbox_setup_failure_message`] in the UI language, for a launcher
/// that shows the failure to the user.
pub fn remote_sandbox_setup_failure_display_message(
    exit_code: Option<u32>,
    output: &str,
) -> Option<String> {
    if exit_code != Some(REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE) {
        return None;
    }
    let code = sandbox_failure_code(output, SandboxHost::Remote)?;
    Some(sandbox_failure_message_for_code(
        SandboxHost::Remote,
        code,
        Reader::User,
    ))
}

/// ADR-F49: the same detection for the Windows backend's helper inside WSL, for
/// callers that run it themselves — an interactive session on a terminal,
/// whose output is the terminal's screen. `output` starts at the marker line.
/// Returns the reason with the fix to try, when there is one.
pub fn wsl_sandbox_setup_failure_message(exit_code: Option<u32>, output: &str) -> Option<String> {
    if exit_code != Some(REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE) {
        return None;
    }
    let code = sandbox_failure_code(output, SandboxHost::Wsl)?;
    let reason = sandbox_failure_message_for_code(SandboxHost::Wsl, code, Reader::User);
    Some(match SandboxHost::Wsl.hint(code) {
        Some(hint) => format!("{reason}\n{}", Reader::User.text(hint)),
        None => reason,
    })
}

/// Writes the fail-closed reason of the sandbox helper — on an SSH host, or
/// inside WSL for the Windows backend — into a finished command's output, so
/// every reader of that output learns why the command never ran, not just the
/// callers that run the detection themselves. The helper's read-hiding
/// warning is explained the same way whatever the exit code, since the command
/// did run.
///
/// The explanation goes *after* the marker line, not in front of it, because
/// the marker has to stay first for [`remote_sandbox_setup_failure_message`] to
/// keep recognizing it: a reader that does its own detection must not be
/// silently downgraded to the generic "failed with exit code 126".
///
/// Returns `output` untouched when neither is there, which is every ordinary
/// command.
pub fn annotate_sandbox_setup_failure(output: String, exit_code: Option<u32>) -> String {
    let output = explain_read_hiding_warning(output);
    if exit_code != Some(REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE) {
        return output;
    }
    let Some((host, code)) = [SandboxHost::Remote, SandboxHost::Wsl]
        .into_iter()
        .find_map(|host| Some((host, sandbox_failure_code(&output, host)?)))
    else {
        return output;
    };
    let reason = sandbox_failure_message_for_code(host, code, Reader::Agent);
    if output.contains(&reason) {
        return output;
    }
    let hint = host
        .hint(code)
        .map(|hint| format!("{hint}\n"))
        .unwrap_or_default();
    let (preamble, rest) = split_sandbox_preamble(&output);
    let (marker_line, rest) = match rest.split_once('\n') {
        Some((marker_line, rest)) => (marker_line, rest),
        None => (rest, ""),
    };
    format!("{preamble}{marker_line}\n{reason}\n{hint}{rest}")
}

/// Whether the helper said, ahead of the command's output, that it could not
/// hide the credential stores from the command, which then ran without that.
pub fn sandbox_read_hiding_unavailable(output: &str) -> bool {
    read_hiding_warning_end(output, READ_HIDING_UNAVAILABLE_WARNING).is_some()
}

/// ADR-F49 decision 11: whether the helper said that it hid the credential
/// stores with Landlock instead, the host having refused the mounts. The
/// command ran protected, but without being able to list the directories on the
/// way down to a hidden entry.
pub fn sandbox_read_hiding_by_landlock(output: &str) -> bool {
    read_hiding_warning_end(output, READ_HIDING_BY_LANDLOCK_WARNING).is_some()
}

/// Puts the note for the helper's read-hiding warning right after it, once.
fn explain_read_hiding_warning(output: String) -> String {
    let (preamble, _) = split_sandbox_preamble(&output);
    for (warning, note) in READ_HIDING_WARNINGS {
        if preamble.contains(note) {
            continue;
        }
        let Some(warning_end) = read_hiding_warning_end(&output, warning) else {
            continue;
        };
        let (before, after) = output.split_at(warning_end);
        let line_break = if before.ends_with('\n') { "" } else { "\n" };
        return format!("{before}{line_break}{note}\n{after}");
    }
    output
}

/// Where the line holding the helper's `warning` ends, when the warnings ahead
/// of the output include it.
fn read_hiding_warning_end(output: &str, warning: &str) -> Option<usize> {
    let (preamble, _) = split_sandbox_preamble(output);
    let mut end = 0;
    for line in preamble.split_inclusive('\n') {
        end += line.len();
        let is_read_hiding = sandbox_warning(line.trim()).is_some_and(|reported| {
            reported
                .split_once(':')
                .is_some_and(|(code, _)| code.trim() == warning)
        });
        if is_read_hiding {
            return Some(end);
        }
    }
    None
}

/// Splits `output` after what comes ahead of everything else — blank lines, at
/// most [`MAX_LEADING_SANDBOX_WARNINGS`] of the helper's warning lines, and
/// the note Zed adds after one — so that what follows is the fail-closed
/// marker line, if there is one, or the command's own output.
fn split_sandbox_preamble(output: &str) -> (&str, &str) {
    let mut rest = output.trim_start();
    let mut warnings = 0;
    let mut explained = false;
    loop {
        let (line, after) = rest.split_once('\n').unwrap_or((rest, ""));
        let line = line.trim();
        if warnings < MAX_LEADING_SANDBOX_WARNINGS && sandbox_warning(line).is_some() {
            warnings += 1;
        } else if !explained && READ_HIDING_WARNINGS.iter().any(|(_, note)| line == *note) {
            explained = true;
        } else {
            break;
        }
        rest = after;
    }
    output.split_at(output.len() - rest.len())
}

/// The text of one of the helper's warning lines, after its prefix.
fn sandbox_warning(line: &str) -> Option<&str> {
    SANDBOX_WARNING_PREFIXES
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .map(str::trim_start)
}

/// Extract `<code>` from `host`'s `<marker> <code>: <detail>` line at the start
/// of `output`, after leading blank lines and the helper's warning lines.
/// Returns `None` when the marker is absent or no `<code>:` follows it, so the
/// detection cannot fire on unrelated terminal output.
fn sandbox_failure_code(output: &str, host: SandboxHost) -> Option<&str> {
    let (_, rest) = split_sandbox_preamble(output);
    let line = rest.lines().next()?;
    let rest = line.strip_prefix(host.marker())?;
    // Codes contain no colon, so the first `:` ends the code and begins the
    // (ignored) human-readable detail.
    let (code, _detail) = rest.split_once(':')?;
    let code = code.trim();
    if code.is_empty() {
        return None;
    }
    Some(code)
}

/// Map a helper failure `<code>` to its user-facing message (GLOSSARY §5),
/// prefixed with `host`'s "could not create the sandbox" lead-in. An
/// unrecognized code falls back to naming the raw code (`Unknown reason
/// (<code>)`) so a future helper addition still surfaces something actionable
/// rather than nothing.
fn sandbox_failure_message_for_code(host: SandboxHost, code: &str, reader: Reader) -> String {
    let reason = match (host, code) {
        (SandboxHost::Remote, "slirp-missing") => i18n::msg!(
            "The sandbox helper (slirp4netns) is missing from `~/.zed_server/` on the remote \
             host, so network access can't be isolated. Reconnecting puts it back."
        ),
        (SandboxHost::Wsl, "slirp-missing") => i18n::msg!(
            "The sandbox helper (slirp4netns) wasn't found inside WSL, so network access can't \
             be isolated. Restarting Zed puts it back."
        ),
        (SandboxHost::Remote, "userns-failed") => i18n::msg!(
            "User namespaces can't be created (they may be disabled). In a dev container \
             (Docker), the default seccomp profile refuses `unshare`, so add \
             `--cap-add SYS_ADMIN` (or `--security-opt seccomp=unconfined`) to `runArgs`, and \
             `--device /dev/net/tun` for commands that use the network."
        ),
        (SandboxHost::Wsl, "userns-failed") => {
            i18n::msg!("User namespaces can't be created in WSL.")
        }
        (SandboxHost::Remote, "no-fs-backend") => {
            i18n::msg!("Neither file isolation method (Landlock or mounts) is available.")
        }
        (SandboxHost::Wsl, "no-fs-backend") => i18n::msg!("WSL's kernel doesn't support Landlock."),
        (_, "landlock-failed" | "mount-failed") => {
            i18n::msg!("The file write restrictions couldn't be applied.")
        }
        (_, "protected-pin-failed") => i18n::msg!(
            "The protected paths (credentials and shell startup files such as `.git`, \
             `~/.ssh` and `~/.bashrc`) couldn't be checked (check their permissions)."
        ),
        (SandboxHost::Remote, "home-unknown") => i18n::msg!(
            "The remote home directory couldn't be determined (the command isn't run, because \
             the paths that are always protected can't be settled)."
        ),
        (SandboxHost::Wsl, "home-unknown") => i18n::msg!(
            "The WSL home directory couldn't be determined (the command isn't run, because the \
             paths that are always protected can't be settled)."
        ),
        (_, "writable-redirected") => i18n::msg!(
            "A path allowed for writing was replaced after the approval so that it points \
             somewhere else (through a symbolic link, for example). The command wasn't run, to be \
             safe."
        ),
        (_, "proxy-failed" | "slirp-failed" | "relay-failed") => {
            i18n::msg!("Network control couldn't be set up.")
        }
        (_, "seccomp-failed") => i18n::msg!("The system call restrictions couldn't be applied."),
        (_, "caps-failed") => i18n::msg!("The command's capabilities couldn't be dropped."),
        (_, "helper-mismatch") => i18n::msg!(
            "The sandbox helper (or slirp4netns) doesn't match the one bundled with this Zed (it \
             was replaced or is damaged). The command wasn't run, to be safe. Restarting Zed puts \
             it back."
        ),
        (SandboxHost::Remote, "subreaper-failed") => i18n::msg!(
            "The remote kernel refused to let the helper reap child processes \
             (PR_SET_CHILD_SUBREAPER), so processes left behind when the command ends can't be \
             cleaned up. The command wasn't run, to be safe."
        ),
        (SandboxHost::Wsl, "subreaper-failed") => i18n::msg!(
            "WSL's kernel refused to let the helper reap child processes \
             (PR_SET_CHILD_SUBREAPER), so processes left behind when the command ends can't be \
             cleaned up. The command wasn't run, to be safe."
        ),
        (_, "signals-failed") => i18n::msg!(
            "The sandbox helper's signal handling couldn't be set up. The command wasn't run, to \
             be safe."
        ),
        (_, "ide-forward-invalid") => i18n::msg!(
            "The folder (or socket) that carries the IDE integration isn't in a safe state (its \
             permissions aren't 0700, it has another owner, or it lies outside the protected \
             paths), so the command wasn't run. Restart Zed and try again."
        ),
        (SandboxHost::Remote, "pidns-failed") => i18n::msg!(
            "The remote kernel refused to create a PID namespace, so processes left behind when \
             the command ends can't be reliably stopped. The command wasn't run, to be safe (a \
             `user.max_pid_namespaces=0` setting or a container's seccomp profile can cause this)."
        ),
        (SandboxHost::Wsl, "pidns-failed") => i18n::msg!(
            "WSL's kernel refused to create a PID namespace, so processes left behind when the \
             command ends can't be reliably stopped. The command wasn't run, to be safe (a \
             `user.max_pid_namespaces=0` setting can cause this)."
        ),
        (_, "uidmap-failed") => i18n::msg!(
            "The user ID mapping (uid_map / gid_map) couldn't be set up in the user namespace."
        ),
        (_, "windows-home-root") => i18n::msg!(
            "A folder allowed for writing is your Windows user folder itself or contains it, which \
             the sandbox can't use (it keeps the credentials inside it from being read). Open the \
             project from a folder below it."
        ),
        (_, "readable-refused") => i18n::msg!(
            "A path the agent asked to see read-only, as something it needs to run, is neither \
             the agent's install folder (`external_agents`) nor Node.js (`node`) in Zed's data \
             folder (or it overlaps credentials), so the command wasn't run."
        ),
        (
            _,
            "pipe-failed" | "fork-failed" | "wait-failed" | "handshake-failed" | "mkdtemp-failed",
        ) => {
            let reason = match reader {
                Reader::Agent => {
                    format!("An internal error occurred while preparing the sandbox ({code}).")
                }
                Reader::User => {
                    i18n::t!("An internal error occurred while preparing the sandbox ({code}).")
                }
            };
            return format!("{}{reason}", host.prefix(reader));
        }
        (_, "exec-failed" | "chdir-failed") => {
            i18n::msg!("The command couldn't be started inside the sandbox.")
        }
        (_, "bad-args") => {
            i18n::msg!("The sandbox's launch arguments are invalid (internal error).")
        }
        (_, other) => {
            let reason = match reader {
                Reader::Agent => format!("Unknown reason ({other})"),
                Reader::User => i18n::t!("Unknown reason ({other})"),
            };
            return format!("{}{reason}", host.prefix(reader));
        }
    };
    format!("{}{}", host.prefix(reader), reader.text(reason))
}

/// ADR-F17: the editor-wide network allowlist that a sandbox helper's proxy
/// nests a command's own network inside, for the helpers that cannot consult
/// the live policy object themselves: the WSL one, whose proxy runs inside the
/// distro (see `sandbox::Sandbox::set_editor_network_allowlist`), and a remote
/// host's, whose proxy runs on the far side of the connection (see
/// [`build_remote_sandbox_command`]).
///
/// A type of its own rather than a bare `Option<Vec<String>>`, because the
/// inner `None` — this process publishes no allowlist (an embedder, a test) —
/// has to stay distinguishable from a call site that passed nothing: only
/// [`SandboxCeiling::read`] makes one, so every path from
/// [`super::AcpThread::create_terminal`] to a helper's argv carries the
/// allowlist the app actually holds, and one that forgot it does not compile.
///
/// Read when the command is built, so editing the allowlist re-aims the
/// ceiling of the next command rather than of one already running — the same
/// bound the WSL backend has.
#[derive(Clone, Debug)]
pub(crate) struct SandboxCeiling(Option<Vec<String>>);

impl SandboxCeiling {
    pub(crate) fn read(cx: &App) -> Self {
        Self(
            cx.try_global::<egress_policy::EgressAllowlistGlobal>()
                .map(|global| {
                    global
                        .0
                        .snapshot()
                        .patterns()
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                }),
        )
    }

    pub(crate) fn entries(&self) -> Option<&[String]> {
        self.0.as_deref()
    }
}

/// ADR-F18 + ADR-F17: the terminal spawn that runs one of the agent's commands
/// inside a remote host's sandbox helper, under the editor-wide allowlist as
/// its ceiling. Assembled here rather than at the call site so that what the
/// helper is told can be checked without a remote host to tell it to.
pub(crate) fn remote_sandboxed_spawn(
    remote_binary: &RelPath,
    wrap: &SandboxWrap,
    ceiling: &SandboxCeiling,
    cwd: Option<&Path>,
    environment: &HashMap<String, String>,
    inner_command: String,
    inner_args: Vec<String>,
) -> task::SpawnInTerminal {
    let (command, args) = build_remote_sandbox_command(
        remote_binary,
        wrap,
        ceiling.entries(),
        cwd,
        environment,
        &[],
        false,
        // A one-off command has no IDE channel of its own.
        None,
        // A one-off command is followed by the turn that started it, which
        // does not end before it does.
        None,
        inner_command,
        inner_args,
    );
    task::SpawnInTerminal {
        command: Some(command),
        args,
        // `cwd: None`: the helper's path is relative to the remote home, where
        // the transport starts it, and it `--chdir`s itself.
        cwd: None,
        ..Default::default()
    }
}

/// The sandbox a command is about to be wrapped in, with the editor-wide
/// allowlist handed to it on Windows (ADR-F17): the WSL helper's proxy runs
/// inside the distro, where the live policy object cannot follow, so the
/// entries travel with the command. A separate step so that what the caller
/// assembles for it can be checked without WSL.
fn sandbox_for_wrap(
    sandbox_wrap: &SandboxWrap,
    #[cfg(target_os = "windows")] ceiling: &SandboxCeiling,
) -> anyhow::Result<sandbox::Sandbox> {
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut sandbox =
        sandbox::Sandbox::new(sandbox_wrap.to_policy()?).map_err(anyhow::Error::new)?;
    #[cfg(target_os = "windows")]
    if let Some(entries) = ceiling.entries() {
        sandbox.set_editor_network_allowlist(entries.to_vec());
    }
    Ok(sandbox)
}

pub(crate) async fn prepare_sandbox_wrap(
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    sandbox_wrap: Option<SandboxWrap>,
    env: HashMap<String, String>,
    #[cfg(target_os = "windows")] ceiling: SandboxCeiling,
) -> anyhow::Result<(
    String,
    Vec<String>,
    HashMap<String, String>,
    Option<SandboxConfigHandle>,
)> {
    let Some(sandbox_wrap) = sandbox_wrap else {
        return Ok((program, args, env, None));
    };

    // Why the command could not be sandboxed is the agent's to read: the
    // terminal it asked for, native or over ACP, fails with it. A WSL
    // environment failure also carries the UI language for the retry card
    // (`sandbox::SandboxError::WslUnavailable`).
    let mut sandbox = i18n::in_english(|| {
        sandbox_for_wrap(
            &sandbox_wrap,
            #[cfg(target_os = "windows")]
            &ceiling,
        )
    })?;
    let command = sandbox::CommandAndArgs {
        program,
        args,
        env: env.into_iter().collect::<StdHashMap<_, _>>(),
        cwd,
    };
    let wrapped = i18n::in_english_while_polling(sandbox.wrap(&command))
        .await
        .map_err(anyhow::Error::new)?;
    Ok((
        wrapped.program,
        wrapped.args,
        wrapped.env.into_iter().collect(),
        Some(sandbox),
    ))
}

pub struct Terminal {
    id: acp_v2::TerminalId,
    command: Entity<Markdown>,
    working_dir: Option<PathBuf>,
    terminal: Entity<terminal::Terminal>,
    started_at: Instant,
    output: Option<TerminalOutput>,
    output_byte_limit: Option<usize>,
    execution: TerminalExecution,
    /// Flag indicating whether this terminal was stopped by explicit user action
    /// (e.g., clicking the Stop button). This is set before kill() is called
    /// so that code awaiting wait_for_exit() can check it deterministically.
    user_stopped: Arc<AtomicBool>,
    /// The live sandbox (Seatbelt policy file and/or network proxy) kept alive
    /// until the sandboxed command exits. `None` when the command isn't
    /// sandboxed or after it finishes. Dropping it tears down the proxy on a
    /// background thread (see `sandbox::Sandbox`'s `Drop`).
    _sandbox: Option<SandboxConfigHandle>,
    /// ADR-F50: keeps the command registered as a writer of the project's
    /// worktrees until it exits.
    worktree_code_writer: Option<project::SandboxedWriterGuard>,
}

enum TerminalExecution {
    Process(Shared<Task<acp_v2::TerminalExitStatus>>),
    Display(DisplayTerminalState),
}

#[derive(Debug, Default)]
pub struct DisplayTerminalState {
    command: DisplayCommand,
    pub meta: Option<acp_v2::Meta>,
    pub output_meta: Option<acp_v2::Meta>,
    legacy_created: bool,
    legacy_exited: bool,
}

#[derive(Debug, Default)]
enum DisplayCommand {
    #[default]
    Unknown,
    LegacyCaption(String),
    Reported(Option<String>),
}

impl DisplayTerminalState {
    pub fn command(&self) -> Option<&str> {
        match &self.command {
            DisplayCommand::LegacyCaption(command) | DisplayCommand::Reported(Some(command)) => {
                Some(command)
            }
            DisplayCommand::Unknown | DisplayCommand::Reported(None) => None,
        }
    }
}

/// Terminal patches use decoded bytes; decoding and validating wire data belongs to the adapter.
#[derive(Debug, Default)]
pub struct DisplayTerminalPatch {
    pub command: MaybeUndefined<String>,
    pub cwd: MaybeUndefined<acp_v2::AbsolutePath>,
    pub output: MaybeUndefined<DisplayTerminalOutput>,
    pub exit_status: MaybeUndefined<acp_v2::TerminalExitStatus>,
    pub meta: MaybeUndefined<acp_v2::Meta>,
}

#[derive(Debug)]
pub struct DisplayTerminalOutput {
    pub data: Vec<u8>,
    pub meta: Option<acp_v2::Meta>,
}

/// Process snapshots are separate from agent-reported display output and wire response envelopes.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessOutput {
    pub output: String,
    pub truncated: bool,
    pub exit_status: Option<acp_v2::TerminalExitStatus>,
}

impl ProcessOutput {
    pub fn new(output: impl Into<String>, truncated: bool) -> Self {
        Self {
            output: output.into(),
            truncated,
            exit_status: None,
        }
    }

    pub fn exit_status(mut self, status: acp_v2::TerminalExitStatus) -> Self {
        self.exit_status = Some(status);
        self
    }
}

pub struct TerminalOutput {
    pub ended_at: Instant,
    pub exit_status: acp_v2::TerminalExitStatus,
    pub content: String,
    pub original_content_len: usize,
    pub content_line_count: usize,
}

impl Terminal {
    pub fn new(
        id: acp_v2::TerminalId,
        command_label: &str,
        working_dir: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        terminal: Entity<terminal::Terminal>,
        language_registry: Arc<LanguageRegistry>,
        sandbox: Option<SandboxConfigHandle>,
        worktree_code_writer: Option<project::SandboxedWriterGuard>,
        cx: &mut Context<Self>,
    ) -> Self {
        let command_task = terminal.read(cx).wait_for_completed_task(cx);
        // Tear the sandbox down on a GPUI background thread when this entity is
        // released, rather than relying on `Sandbox`'s `Drop` (which would spawn
        // a throwaway thread) on whatever thread releases us. `on_release` hands
        // us an `App`, so we can drive the teardown through the background
        // executor with `drop_on_current_thread`.
        cx.on_release(|this, cx| {
            if let Some(sandbox) = this._sandbox.take() {
                cx.background_executor()
                    .spawn(async move { sandbox.drop_on_current_thread() })
                    .detach();
            }
        })
        .detach();
        Self {
            id,
            _sandbox: sandbox,
            worktree_code_writer,
            command: cx.new(|cx| {
                Markdown::new(
                    format!("```\n{}\n```", command_label).into(),
                    Some(language_registry.clone()),
                    None,
                    cx,
                )
            }),
            working_dir,
            terminal,
            started_at: Instant::now(),
            output: None,
            output_byte_limit,
            user_stopped: Arc::new(AtomicBool::new(false)),
            execution: TerminalExecution::Process(
                cx.spawn(async move |this, cx| {
                    let exit_status = command_task.await.map(Self::transform_exit_status);
                    let exit_status = acp_v2::TerminalExitStatus::new()
                        .exit_code(exit_status.as_ref().map(|status| status.0))
                        .signal(exit_status.and_then(|status| status.1));

                    this.update(cx, |this, cx| {
                        this.cache_output(exit_status.clone(), Instant::now(), cx);

                        // ADR-F18: the helper failed closed, so the command
                        // never ran — or (ADR-F49) it could not hide the
                        // credential stores, and the command ran anyway.
                        // Explaining that here reaches everyone who reads this
                        // terminal's output — external ACP agents included,
                        // which previously got exit 126 and one line of
                        // English marker and had to guess.
                        //
                        // Inserted *after* the marker line rather than in front
                        // of it, so that a reader which recognizes the marker
                        // itself (`terminal_tool`) still does.
                        if let Some(output) = this.output.as_mut() {
                            output.content = annotate_sandbox_setup_failure(
                                std::mem::take(&mut output.content),
                                output.exit_status.exit_code,
                            );
                        }
                        this.terminal.update(cx, |terminal, _cx| {
                            terminal.release_pty_resources();
                        });
                        // Free the sandbox (and its network proxy) as soon as
                        // the command finishes, rather than holding it until
                        // this entity is released. The proxy's teardown joins a
                        // listener thread, so run it on the background executor
                        // to keep it off the foreground thread.
                        if let Some(sandbox) = this._sandbox.take() {
                            cx.background_executor()
                                .spawn(async move { sandbox.drop_on_current_thread() })
                                .detach();
                        }
                        drop(this.worktree_code_writer.take());
                        cx.notify();
                    })
                    .ok();

                    exit_status
                })
                .shared(),
            ),
        }
    }

    pub fn new_display(
        id: acp_v2::TerminalId,
        command_label: Option<&str>,
        working_dir: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        terminal: Entity<terminal::Terminal>,
        language_registry: Arc<LanguageRegistry>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            id,
            command: cx.new(|cx| {
                Markdown::new(
                    format!("```\n{}\n```", command_label.unwrap_or("Terminal")).into(),
                    Some(language_registry),
                    None,
                    cx,
                )
            }),
            working_dir,
            terminal,
            started_at: Instant::now(),
            output: None,
            output_byte_limit,
            execution: TerminalExecution::Display(DisplayTerminalState {
                command: command_label
                    .map(|command| DisplayCommand::LegacyCaption(command.to_owned()))
                    .unwrap_or_default(),
                ..Default::default()
            }),
            user_stopped: Arc::new(AtomicBool::new(false)),
            _sandbox: None,
            worktree_code_writer: None,
        }
    }

    pub fn id(&self) -> &acp_v2::TerminalId {
        &self.id
    }

    /// Ownership is independent of the renderer's PTY: headless tasks still own a process.
    pub fn is_process_backed(&self) -> bool {
        matches!(self.execution, TerminalExecution::Process(_))
    }

    pub fn wait_for_exit(&self) -> Result<Shared<Task<acp_v2::TerminalExitStatus>>> {
        match &self.execution {
            TerminalExecution::Process(task) => Ok(task.clone()),
            TerminalExecution::Display(_) => {
                bail!("Agent-provided terminals have no client-owned process to wait for")
            }
        }
    }

    pub fn kill(&mut self, cx: &mut App) {
        if !self.is_process_backed() {
            return;
        }
        self.terminal.update(cx, |terminal, _cx| {
            terminal.kill_active_task();
        });
    }

    /// Marks this terminal as stopped by user action and then kills it.
    /// This should be called when the user explicitly clicks a Stop button.
    pub fn stop_by_user(&mut self, cx: &mut App) {
        if !self.is_process_backed() {
            return;
        }
        self.user_stopped.store(true, Ordering::SeqCst);
        self.kill(cx);
    }

    /// Returns whether this terminal was stopped by explicit user action.
    pub fn was_stopped_by_user(&self) -> bool {
        self.user_stopped.load(Ordering::SeqCst)
    }

    pub fn current_output(&self, cx: &App) -> ProcessOutput {
        if let Some(output) = self.output.as_ref() {
            ProcessOutput::new(
                output.content.clone(),
                output.original_content_len > output.content.len(),
            )
            .exit_status(output.exit_status.clone())
        } else {
            let (current_content, original_len) = self.truncated_output(cx);
            let truncated = current_content.len() < original_len;
            ProcessOutput::new(current_content, truncated)
        }
    }

    pub(crate) fn write_display_output(&mut self, data: &[u8], cx: &mut Context<Self>) {
        if self.is_process_backed() {
            return;
        }
        self.terminal
            .update(cx, |terminal, cx| terminal.write_output(data, cx));
        self.refresh_output(cx);
    }

    fn refresh_output(&mut self, cx: &mut Context<Self>) {
        if let Some(output) = &self.output {
            // A process may exit before its final output arrives. Refresh the
            // cached content without changing its completion time or status.
            self.cache_output(output.exit_status.clone(), output.ended_at, cx);
        }
    }

    pub(crate) fn finish_display(
        &mut self,
        exit_status: acp_v1::TerminalExitStatus,
        cx: &mut Context<Self>,
    ) {
        let TerminalExecution::Display(state) = &mut self.execution else {
            return;
        };
        if std::mem::replace(&mut state.legacy_exited, true) || self.output.is_some() {
            return;
        }
        self.terminal
            .update(cx, |terminal, _| terminal.shrink_to_used());
        self.cache_output(Self::exit_status_from_v1(exit_status), Instant::now(), cx);
    }

    fn exit_status_from_v1(status: acp_v1::TerminalExitStatus) -> acp_v2::TerminalExitStatus {
        acp_v2::TerminalExitStatus::new()
            .exit_code(status.exit_code)
            .signal(status.signal)
            .meta(status.meta)
    }

    pub fn display_state(&self) -> Option<&DisplayTerminalState> {
        match &self.execution {
            TerminalExecution::Display(state) => Some(state),
            TerminalExecution::Process(_) => None,
        }
    }

    pub(crate) fn initialize_legacy_display(
        &mut self,
        command: &str,
        cwd: Option<PathBuf>,
        output_byte_limit: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let TerminalExecution::Display(state) = &mut self.execution else {
            return;
        };
        if std::mem::replace(&mut state.legacy_created, true) {
            return;
        }
        self.update_command_label(command, cx);
        self.working_dir = cwd;
        if self.output_byte_limit != output_byte_limit {
            self.output_byte_limit = output_byte_limit;
            self.refresh_output(cx);
        }
        cx.notify();
    }

    pub(crate) fn append_display_bytes(
        &mut self,
        data: &[u8],
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.is_process_backed(),
            "Cannot patch a client-owned terminal"
        );
        self.terminal
            .update(cx, |terminal, cx| terminal.write_raw_output(data, cx));
        self.refresh_output(cx);
        cx.notify();
        Ok(())
    }

    pub(crate) fn apply_display_patch(
        &mut self,
        patch: DisplayTerminalPatch,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.is_process_backed(),
            "Cannot patch a client-owned terminal"
        );
        let DisplayTerminalPatch {
            command,
            cwd,
            output,
            exit_status,
            meta,
        } = patch;
        let output_changed = !output.is_undefined();
        let output_meta = match output {
            MaybeUndefined::Undefined => None,
            MaybeUndefined::Null => {
                self.terminal
                    .update(cx, |terminal, cx| terminal.replace_display_output(&[], cx))?;
                Some(None)
            }
            MaybeUndefined::Value(output) => {
                self.terminal.update(cx, |terminal, cx| {
                    terminal.replace_display_output(&output.data, cx)
                })?;
                Some(output.meta)
            }
        };

        if let TerminalExecution::Display(state) = &mut self.execution {
            if !command.is_undefined() {
                state.command = DisplayCommand::Reported(command.take());
                self.command.update(cx, |markdown, cx| {
                    markdown.replace(
                        format!("```\n{}\n```", state.command().unwrap_or("Terminal")),
                        cx,
                    );
                });
            }
            if !meta.is_undefined() {
                state.meta = meta.take();
            }
            if let Some(meta) = output_meta {
                state.output_meta = meta;
            }
        }
        if !cwd.is_undefined() {
            self.working_dir = cwd.take().map(|path| path.0);
        }
        match exit_status {
            MaybeUndefined::Undefined => {
                if output_changed {
                    self.refresh_output(cx);
                }
            }
            MaybeUndefined::Null => self.output = None,
            MaybeUndefined::Value(status) => {
                let ended_at = self
                    .output
                    .as_ref()
                    .map_or_else(Instant::now, |output| output.ended_at);
                self.terminal
                    .update(cx, |terminal, _| terminal.shrink_to_used());
                self.cache_output(status, ended_at, cx);
            }
        }
        cx.notify();
        Ok(())
    }

    fn cache_output(
        &mut self,
        exit_status: acp_v2::TerminalExitStatus,
        ended_at: Instant,
        cx: &mut Context<Self>,
    ) {
        let (content, original_content_len) = self.truncated_output(cx);
        let content_line_count = self.terminal.read(cx).total_lines();
        self.output = Some(TerminalOutput {
            ended_at,
            exit_status,
            content,
            original_content_len,
            content_line_count,
        });
        cx.notify();
    }

    fn truncated_output(&self, cx: &App) -> (String, usize) {
        let terminal = self.terminal.read(cx);
        let mut content = terminal.get_content();

        let original_content_len = content.len();

        if let Some(limit) = self.output_byte_limit
            && content.len() > limit
        {
            let mut end_ix = limit.min(content.len());
            while !content.is_char_boundary(end_ix) {
                end_ix -= 1;
            }
            // Don't truncate mid-line, clear the remainder of the last line
            end_ix = content[..end_ix].rfind('\n').unwrap_or(end_ix);
            content.truncate(end_ix);
        }

        (content, original_content_len)
    }

    pub fn command(&self) -> &Entity<Markdown> {
        &self.command
    }

    pub fn update_command_label(&mut self, label: &str, cx: &mut App) {
        if let TerminalExecution::Display(state) = &mut self.execution {
            if matches!(state.command, DisplayCommand::Reported(_))
                || state.command() == Some(label)
            {
                return;
            }
            state.command = DisplayCommand::LegacyCaption(label.to_owned());
        }
        self.command.update(cx, |command, cx| {
            command.replace(format!("```\n{}\n```", label), cx);
        });
    }

    pub fn working_dir(&self) -> &Option<PathBuf> {
        &self.working_dir
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn output(&self) -> Option<&TerminalOutput> {
        self.output.as_ref()
    }

    pub fn inner(&self) -> &Entity<terminal::Terminal> {
        &self.terminal
    }

    pub fn to_markdown(&self, cx: &App) -> String {
        format!(
            "Terminal:\n```\n{}\n```\n",
            self.terminal.read(cx).get_content()
        )
    }

    // This method is adapted from `portable_pty::ExitStatus::from`, version 0.9.0
    fn transform_exit_status(exit_status: ExitStatus) -> (u32, Option<String>) {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;

            if let Some(signal) = exit_status.signal() {
                let signame = unsafe { libc::strsignal(signal) };
                let signal = if signame.is_null() {
                    format!("Signal {}", signal)
                } else {
                    let signame = unsafe { std::ffi::CStr::from_ptr(signame) };
                    signame.to_string_lossy().to_string()
                };

                return (
                    exit_status.code().map(|c| c as u32).unwrap_or(1),
                    Some(signal),
                );
            }
        }

        let code = exit_status
            .code()
            .map(|c| c as u32)
            .unwrap_or_else(|| if exit_status.success() { 0 } else { 1 });

        (code, None)
    }
}

/// The directory subtrees the sandbox always grants write access to for a
/// project: the roots of the worktrees the user opened. This is the single
/// source of truth shared by the agents' terminals (which hand these to the
/// sandbox as [`SandboxWrap::writable_paths`]), the write scope an external
/// agent's process is registered with (ADR-F49 decision 18) and the status UI
/// (which lists them), so the three can't drift if the set ever changes.
///
/// Only the visible worktrees: going to a definition in `~/.cargo/registry`, a
/// file dropped on the window or one a bookmark opened each add a worktree of
/// their own, which the project panel doesn't show and the user never offered
/// to the agent — those stay read-only (ADR-F46, ADR-F49).
pub fn sandbox_worktree_writable_paths(project: &Project, cx: &App) -> Vec<PathBuf> {
    project
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

/// The candidate `.git` paths the sandbox protects for a project. Locating these
/// requires Git knowledge the sandbox layer can't derive itself: a worktree's
/// `.git`, a linked worktree's common dir (which lives outside the worktree),
/// and every discovered repository's git/common dirs.
pub fn sandbox_git_dirs(project: &Project, cx: &App) -> Vec<PathBuf> {
    let mut git_dirs = Vec::new();
    let path_style = project.path_style(cx);

    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let worktree_abs_path = worktree.abs_path();
        // Protect `<worktree>/.git` even when it doesn't exist yet, so a command
        // can't `git init` and then write to the freshly created metadata. On
        // Linux a missing one can't be captured and is dropped by
        // `SandboxWrap::to_policy`; `sandbox::Sandbox::new` adds it back for a
        // writable root (see `SandboxWrap::to_policy`).
        //
        // We don't gate this on the worktree's scanned root being a directory:
        // that state can be stale/pending, and for a *single-file* worktree
        // (e.g. `settings.json` opened on its own) this synthesizes
        // `settings.json/.git`, which can never exist. That impossible path is
        // resolved authoritatively at capture time — the real filesystem
        // reports `NotADirectory`, and `SandboxWrap::to_policy` skips a
        // protected path that can't exist — so it never reaches enforcement.
        git_dirs.push(dot_git_path(&worktree_abs_path, path_style));
        if let Some(root_repo_common_dir) = worktree.root_repo_common_dir() {
            git_dirs.push(root_repo_common_dir.to_path_buf());
        }
    }

    for repository in project.git_store().read(cx).repositories().values() {
        let repository = repository.read(cx);
        git_dirs.push(repository.dot_git_abs_path.to_path_buf());
        git_dirs.push(repository.repository_dir_abs_path.to_path_buf());
        git_dirs.push(repository.common_dir_abs_path.to_path_buf());
    }

    git_dirs.sort();
    git_dirs.dedup();
    git_dirs
}

/// `<root>/.git` spelled the way the machine that owns `root` spells its paths.
///
/// `Path::join` always writes the separator of the machine Zed itself runs on,
/// which for a remote project is the wrong machine: on a Windows client an SSH
/// host's `/srv/app` becomes `/srv/app\.git`, a name no Linux host has. A
/// protected path that cannot exist is skipped when the policy is built
/// (`SandboxWrap::to_policy`, and the helper's own `classify_protected_paths`),
/// so the repository's metadata would have been left writable to every
/// sandboxed command on the host. The project's own style spells it as the host
/// does; a path this cannot render (not UTF-8) keeps the local spelling rather
/// than losing the protection.
fn dot_git_path(root: &Path, path_style: PathStyle) -> PathBuf {
    path_style
        .join(root, ".git")
        .map_or_else(|| root.join(".git"), PathBuf::from)
}

/// Whether sandboxing is *applicable* for this project at all — the feature is
/// enabled, the platform has a sandbox integration, and the project is either
/// local or (ADR-F18) a remote Linux host (over SSH, WSL or Docker) whose remote
/// server can enforce the sandbox on its side. Independent of the persistent
/// `allow_unsandboxed` setting. Used by the UI to distinguish "sandboxing isn't
/// relevant here" (don't show the indicator) from "sandboxing is available but
/// turned off in settings" (show it, struck out).
pub fn sandboxing_available_for_project(project: &Project, cx: &App) -> bool {
    if !cx.has_flag::<SandboxingFeatureFlag>() {
        return false;
    }
    // ADR-F18: a remote Linux project (SSH, WSL or Docker) enforces the sandbox
    // on the remote host, so it is available regardless of the client's own OS.
    if project.remote_supports_agent_sandbox(cx) {
        return true;
    }
    project.is_local()
        && cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        ))
}

/// Why agent terminal commands in this project run outside any sandbox, shown
/// on the approval card that every such command gets (ADR-F46).
pub fn unsandboxed_execution_reason(project: &Project, cx: &App) -> String {
    if sandboxing_available_for_project(project, cx) {
        return i18n::t!("The sandbox is turned off in the settings").to_string();
    }
    let project_kind = if project.is_via_collab() {
        i18n::t!("Collaboration")
    } else if let Some(remote_client) = project.remote_client() {
        let remote_client = remote_client.read(cx);
        match remote_client.connection_type() {
            "wsl" => i18n::t!("WSL remote"),
            "docker" => "Docker",
            "podman" => "Podman",
            "ssh" if remote_client.remote_platform().os.as_str() != "linux" => {
                i18n::t!("SSH (not Linux)")
            }
            "ssh" => "SSH",
            _ => i18n::t!("Remote"),
        }
    } else if !cx.has_flag::<SandboxingFeatureFlag>() {
        i18n::t!("sandboxing is turned off")
    } else {
        i18n::t!("not supported on this OS")
    };
    i18n::t!("This project can't use the sandbox ({project_kind})")
}

// Disable pagers so agent/terminal commands don't hang behind interactive UIs
pub(crate) fn disable_pagers_through_env(env: &mut collections::HashMap<String, String>) {
    env.insert("PAGER".into(), "".into());
    // Override user core.pager (e.g. delta) which Git prefers over PAGER
    env.insert("GIT_PAGER".into(), "cat".into());
}

#[cfg(test)]
mod remote_sandbox_failure_tests {
    use super::*;

    /// The complete set of failure codes the helper can emit (mirrors
    /// `sandbox::linux_remote_helper`). Kept here so the test breaks if a code
    /// is added without a translation.
    const ALL_CODES: &[&str] = &[
        "slirp-missing",
        "userns-failed",
        "no-fs-backend",
        "landlock-failed",
        "mount-failed",
        "protected-pin-failed",
        "proxy-failed",
        "slirp-failed",
        "relay-failed",
        "seccomp-failed",
        "exec-failed",
        "chdir-failed",
        "bad-args",
        "home-unknown",
        "writable-redirected",
        "caps-failed",
        "helper-mismatch",
        "subreaper-failed",
        "signals-failed",
        "ide-forward-invalid",
        "pidns-failed",
        "uidmap-failed",
        "windows-home-root",
        "pipe-failed",
        "fork-failed",
        "wait-failed",
        "handshake-failed",
        "mkdtemp-failed",
        "readable-refused",
    ];

    /// Every known code maps to a concrete Japanese message carrying the shared
    /// prefix, and none falls through to the unknown-code branch.
    #[test]
    fn every_known_code_has_a_translation() {
        for code in ALL_CODES {
            let output = format!("zed-remote-sandbox: {code}: some detail\n");
            let message = remote_sandbox_setup_failure_message(Some(126), &output)
                .unwrap_or_else(|| panic!("code {code} should be recognized"));
            assert!(
                message.starts_with(REMOTE_SANDBOX_FAILURE_PREFIX),
                "message for {code} must carry the shared prefix: {message}"
            );
            assert!(
                !message.contains("Unknown reason"),
                "known code {code} must not fall back to the unknown branch: {message}"
            );
        }
    }

    /// ADR-F49: an interactive session's screen, read by its launcher, gets the
    /// WSL wording and the fix; anything but the helper's signature is left alone.
    #[test]
    fn wsl_setup_failures_are_recognized_on_their_own() {
        let message = wsl_sandbox_setup_failure_message(
            Some(126),
            "zed-wsl-sandbox: userns-failed: detail\nmore output",
        )
        .expect("the helper's signature is recognized");
        assert!(message.starts_with(WSL_SANDBOX_FAILURE_PREFIX), "{message}");
        assert!(message.ends_with(WSL_SANDBOX_UPDATE_HINT), "{message}");
        assert!(
            wsl_sandbox_setup_failure_message(Some(1), "zed-wsl-sandbox: userns-failed: x")
                .is_none()
        );
        assert!(
            wsl_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: userns-failed: x")
                .is_none()
        );
        assert!(wsl_sandbox_setup_failure_message(Some(126), "claude: exit").is_none());
    }

    /// A few representative codes map to their exact GLOSSARY §5 wording, and
    /// aliased codes share one message.
    #[test]
    fn representative_codes_map_to_exact_wording() {
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: slirp-missing: x"),
            Some(
                "Couldn't create the sandbox on the remote host: \
                 The sandbox helper (slirp4netns) is missing from `~/.zed_server/` on the remote \
                 host, so network access can't be isolated. Reconnecting puts it back."
                    .to_string()
            )
        );
        // `landlock-failed` and `mount-failed` are aliases for one message.
        assert_eq!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "zed-remote-sandbox: landlock-failed: x"
            ),
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: mount-failed: x"),
        );
        // A protected path the helper could not pin is its own reason: the
        // command never ran *because* one of the protected paths could not be
        // verified, which is not the same as the write restriction failing to
        // apply. The helper protects the account's credentials and shell init
        // files (relative to the remote `$HOME`) on top of the project's `.git`,
        // so the wording names those rather than `.git` alone.
        assert_eq!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "zed-remote-sandbox: protected-pin-failed: /home/u/p/.git: Permission denied (os error 13)"
            ),
            Some(
                "Couldn't create the sandbox on the remote host: \
                 The protected paths (credentials and shell startup files such as `.git`, \
                 `~/.ssh` and `~/.bashrc`) couldn't be checked (check their permissions)."
                    .to_string()
            )
        );
        // The helper refuses to run at all when it cannot work out which `$HOME`
        // those built-in protected paths belong to.
        assert_eq!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "zed-remote-sandbox: home-unknown: neither $HOME nor the passwd database …"
            ),
            Some(
                "Couldn't create the sandbox on the remote host: \
                 The remote home directory couldn't be determined (the command isn't run, \
                 because the paths that are always protected can't be settled)."
                    .to_string()
            )
        );
    }

    /// An unknown code falls back to naming the raw code, still prefixed.
    #[test]
    fn unknown_code_falls_back_to_naming_it() {
        assert_eq!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "zed-remote-sandbox: brand-new-code: detail",
            ),
            Some(
                "Couldn't create the sandbox on the remote host: Unknown reason (brand-new-code)"
                    .to_string()
            )
        );
    }

    /// The two conditions are independent: exit 126 without the marker, and the
    /// marker without exit 126, both yield `None` — an ordinary non-zero exit is
    /// never mistaken for a sandbox-creation failure.
    #[test]
    fn requires_both_exit_code_and_marker() {
        // Exit 126 but ordinary output (a real command that happens to exit 126,
        // e.g. "permission denied" from a shell).
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), "bash: ./run: Permission denied\n"),
            None
        );
        // The marker present but a different (or absent) exit code: the helper
        // only exits 126 on this path, so anything else is not it.
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(1), "zed-remote-sandbox: slirp-missing: x"),
            None
        );
        assert_eq!(
            remote_sandbox_setup_failure_message(None, "zed-remote-sandbox: slirp-missing: x"),
            None
        );
        // A command that merely echoes the phrase mid-output (not as the first
        // line) does not trip the detector.
        assert_eq!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "regular line\nzed-remote-sandbox: slirp-missing: x\n",
            ),
            None
        );
        // Empty output with exit 126 is an ordinary failure, not a sandbox one.
        assert_eq!(remote_sandbox_setup_failure_message(Some(126), ""), None);
    }

    /// Leading blank lines before the marker are tolerated, and a malformed
    /// marker (no `<code>:`) does not fire.
    #[test]
    fn tolerates_leading_blank_lines_but_needs_a_code() {
        assert!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "\n\nzed-remote-sandbox: userns-failed: detail",
            )
            .is_some()
        );
        // Marker prefix present but nothing resembling `<code>:` after it.
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: no-colon-here"),
            None
        );
    }

    /// ADR-F18 / ADR-F27: an external ACP agent only ever sees the terminal's
    /// output, so the fail-closed reason has to be in it — while the marker
    /// stays first, so a reader that runs the detection itself still does.
    #[test]
    fn a_fail_closed_run_carries_its_reason_in_the_output() {
        let annotated = annotate_sandbox_setup_failure(
            "zed-remote-sandbox: slirp-missing: exec slirp4netns\n".to_string(),
            Some(126),
        );
        let mut lines = annotated.lines();
        assert_eq!(
            lines.next(),
            Some("zed-remote-sandbox: slirp-missing: exec slirp4netns")
        );
        assert_eq!(
            lines.next(),
            Some(
                "Couldn't create the sandbox on the remote host: \
                 The sandbox helper (slirp4netns) is missing from `~/.zed_server/` on the remote \
                 host, so network access can't be isolated. Reconnecting puts it back."
            )
        );
        assert_eq!(lines.next(), Some(REMOTE_SANDBOX_UNSANDBOX_HINT));
        assert!(
            remote_sandbox_setup_failure_message(Some(126), &annotated).is_some(),
            "the marker has to stay first: {annotated}"
        );

        // An ordinary command is left exactly as it was, whatever it printed.
        let ordinary = "make: *** [all] Error 126\n".to_string();
        assert_eq!(
            annotate_sandbox_setup_failure(ordinary.clone(), Some(126)),
            ordinary
        );
        assert_eq!(
            annotate_sandbox_setup_failure(
                "zed-remote-sandbox: slirp-missing: x\n".to_string(),
                Some(1)
            ),
            "zed-remote-sandbox: slirp-missing: x\n"
        );
    }

    /// ADR-F46: the WSL helper's failures are worded for WSL, every code it can
    /// emit is translated, and only the kernel-related ones point at
    /// `wsl --update`.
    #[test]
    fn wsl_failures_are_worded_for_wsl() {
        for code in ALL_CODES {
            let annotated = annotate_sandbox_setup_failure(
                format!("zed-wsl-sandbox: {code}: detail\n"),
                Some(126),
            );
            let mut lines = annotated.lines();
            assert_eq!(
                lines.next(),
                Some(format!("zed-wsl-sandbox: {code}: detail").as_str())
            );
            let reason = lines.next().unwrap_or_default();
            assert!(
                reason.starts_with(WSL_SANDBOX_FAILURE_PREFIX),
                "{code}: {reason}"
            );
            assert!(!reason.contains("Unknown reason"), "{code}: {reason}");
            assert!(
                !annotated.contains(REMOTE_SANDBOX_UNSANDBOX_HINT),
                "{code} must not get the SSH-host hint"
            );
        }

        let kernel = annotate_sandbox_setup_failure(
            "zed-wsl-sandbox: no-fs-backend: x\n".to_string(),
            Some(126),
        );
        assert!(kernel.contains("Landlock") && kernel.contains(WSL_SANDBOX_UPDATE_HINT));
        let redirected = annotate_sandbox_setup_failure(
            "zed-wsl-sandbox: writable-redirected: x\n".to_string(),
            Some(126),
        );
        assert!(!redirected.contains(WSL_SANDBOX_UPDATE_HINT));

        // The SSH-host detection `terminal_tool` runs does not fire on it, and
        // an ordinary exit code is left alone.
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), "zed-wsl-sandbox: userns-failed: x"),
            None
        );
        assert_eq!(
            annotate_sandbox_setup_failure(
                "zed-wsl-sandbox: userns-failed: x\n".to_string(),
                Some(1)
            ),
            "zed-wsl-sandbox: userns-failed: x\n"
        );
    }

    /// ADR-F46: the SSH-host hint says what the host needs and never suggests
    /// turning the sandbox off; a container gets the flags it needs.
    #[test]
    fn remote_hints_are_about_fixing_the_host() {
        assert!(!REMOTE_SANDBOX_UNSANDBOX_HINT.contains("allow_unsandboxed"));
        assert!(REMOTE_SANDBOX_UNSANDBOX_HINT.contains("slirp4netns"));
        let userns =
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: userns-failed: x")
                .unwrap_or_default();
        assert!(userns.contains("--cap-add SYS_ADMIN") && userns.contains("--device /dev/net/tun"));
    }

    /// The helper's own plumbing failing is reported with its code, so a
    /// report can say which step broke; an unknown code is named on either
    /// host.
    #[test]
    fn internal_failures_name_their_code() {
        for code in [
            "pipe-failed",
            "fork-failed",
            "wait-failed",
            "handshake-failed",
            "mkdtemp-failed",
        ] {
            assert_eq!(
                remote_sandbox_setup_failure_message(
                    Some(126),
                    &format!("zed-remote-sandbox: {code}: x")
                ),
                Some(format!(
                    "{REMOTE_SANDBOX_FAILURE_PREFIX}An internal error occurred while preparing the sandbox ({code})."
                ))
            );
        }
        let unknown = annotate_sandbox_setup_failure(
            "zed-wsl-sandbox: brand-new-code: x\n".to_string(),
            Some(126),
        );
        assert!(
            unknown.contains(&format!(
                "{WSL_SANDBOX_FAILURE_PREFIX}Unknown reason (brand-new-code)"
            )),
            "{unknown}"
        );
    }

    /// A refused PID namespace names what usually causes it, and in WSL
    /// points at updating WSL's kernel.
    #[test]
    fn a_refused_pid_namespace_names_the_usual_causes() {
        let remote =
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: pidns-failed: x")
                .unwrap_or_default();
        assert!(
            remote.contains("user.max_pid_namespaces=0") && remote.contains("seccomp"),
            "{remote}"
        );
        let wsl = annotate_sandbox_setup_failure(
            "zed-wsl-sandbox: pidns-failed: x\n".to_string(),
            Some(126),
        );
        assert!(
            wsl.contains("user.max_pid_namespaces=0") && wsl.contains(WSL_SANDBOX_UPDATE_HINT),
            "{wsl}"
        );
    }

    const READ_HIDING_WARNING: &str =
        "zed-remote-sandbox-warning: read-hiding-unavailable: mount: Operation not permitted";

    /// ADR-F49: a helper that warned and then failed closed is still
    /// recognized, and the reason still goes right after the marker line.
    #[test]
    fn a_failure_after_a_warning_is_recognized() {
        let output = format!("{READ_HIDING_WARNING}\nzed-remote-sandbox: seccomp-failed: x\n");
        let reason = remote_sandbox_setup_failure_message(Some(126), &output)
            .expect("the marker after the warning should be recognized");
        assert!(reason.contains("system call restrictions"), "{reason}");

        let annotated = annotate_sandbox_setup_failure(output, Some(126));
        assert_eq!(
            annotated.lines().collect::<Vec<_>>(),
            [
                READ_HIDING_WARNING,
                READ_HIDING_UNAVAILABLE_NOTE,
                "zed-remote-sandbox: seccomp-failed: x",
                reason.as_str(),
                REMOTE_SANDBOX_UNSANDBOX_HINT,
            ]
        );
        assert_eq!(
            annotate_sandbox_setup_failure(annotated.clone(), Some(126)),
            annotated,
            "annotating twice must not repeat the note or the reason"
        );
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), &annotated),
            Some(reason)
        );

        // Either host's warning prefix is skipped, but only a few of them.
        assert!(
            remote_sandbox_setup_failure_message(
                Some(126),
                "zed-wsl-sandbox-warning: something\nzed-remote-sandbox: caps-failed: x",
            )
            .is_some()
        );
        let many_warnings = format!(
            "{}zed-remote-sandbox: caps-failed: x",
            "zed-remote-sandbox-warning: w\n".repeat(MAX_LEADING_SANDBOX_WARNINGS + 1)
        );
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), &many_warnings),
            None
        );
    }

    /// ADR-F49: the command ran without read hiding, so the warning is
    /// explained whatever the command exited with — once, and only at the top.
    #[test]
    fn a_read_hiding_warning_is_explained_whatever_the_exit_code() {
        let output = format!("{READ_HIDING_WARNING}\nhello\n");
        let explained = format!("{READ_HIDING_WARNING}\n{READ_HIDING_UNAVAILABLE_NOTE}\nhello\n");
        for exit_code in [Some(0), Some(1), Some(126), None] {
            assert_eq!(
                annotate_sandbox_setup_failure(output.clone(), exit_code),
                explained,
                "{exit_code:?}"
            );
        }
        assert_eq!(
            annotate_sandbox_setup_failure(explained.clone(), Some(0)),
            explained
        );
        assert!(sandbox_read_hiding_unavailable(&output));
        assert!(sandbox_read_hiding_unavailable(&explained));

        // The terminal's carriage returns, and a warning with nothing after it.
        assert_eq!(
            annotate_sandbox_setup_failure(format!("{READ_HIDING_WARNING}\r\nhello\r\n"), Some(0)),
            format!("{READ_HIDING_WARNING}\r\n{READ_HIDING_UNAVAILABLE_NOTE}\nhello\r\n")
        );
        assert_eq!(
            annotate_sandbox_setup_failure(READ_HIDING_WARNING.to_string(), Some(0)),
            format!("{READ_HIDING_WARNING}\n{READ_HIDING_UNAVAILABLE_NOTE}\n")
        );

        // The same text in the command's own output is left alone.
        let echoed = format!("hello\n{READ_HIDING_WARNING}\n");
        assert_eq!(
            annotate_sandbox_setup_failure(echoed.clone(), Some(0)),
            echoed
        );
        assert!(!sandbox_read_hiding_unavailable(&echoed));
    }

    /// ADR-F49 decision 11: where the host refused the covers but Landlock hid
    /// the credential stores instead, the note says what that costs — and the
    /// "the command ran with nothing hidden" detection must not fire for it,
    /// since something very much is.
    #[test]
    fn a_landlock_read_hiding_warning_says_what_it_costs() {
        const WARNING: &str = "zed-remote-sandbox-warning: read-hiding-by-landlock: \
             unshare(CLONE_NEWNS): Operation not permitted (os error 1); nothing is hidden; \
             Landlock denies reading them instead, from 71 grants";
        let output = format!("{WARNING}\nhello\n");
        let explained = annotate_sandbox_setup_failure(output.clone(), Some(0));
        assert_eq!(
            explained,
            format!("{WARNING}\n{READ_HIDING_BY_LANDLOCK_NOTE}\nhello\n")
        );
        assert!(explained.contains("ls ~"), "{explained}");
        assert_eq!(
            annotate_sandbox_setup_failure(explained.clone(), Some(0)),
            explained,
            "annotating twice must not repeat the note"
        );
        for text in [&output, &explained] {
            assert!(sandbox_read_hiding_by_landlock(text), "{text}");
            assert!(!sandbox_read_hiding_unavailable(text), "{text}");
        }

        // A fail-closed marker after it is still recognized.
        let failed = annotate_sandbox_setup_failure(
            format!("{WARNING}\nzed-remote-sandbox: seccomp-failed: x\n"),
            Some(126),
        );
        assert!(failed.contains("system call restrictions"), "{failed}");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    /// Regression test for the bug where enforcement-policy construction
    /// *created* missing write grants — famously turning a granted
    /// `~/.config/zed/AGENTS.md` file path into a directory. A grant whose
    /// target no longer exists must fail policy construction with an error
    /// naming it, and nothing may be created — a required safety grant that
    /// can't be honored must stop the command, not silently shrink its access.
    #[test]
    fn to_policy_fails_on_missing_grant_and_never_creates_it() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("AGENTS.md");

        let wrap = SandboxWrap {
            extra_write_paths: vec![settings::GrantedWritePath::resolved(
                missing.clone(),
                missing.clone(),
            )],
            ..Default::default()
        };

        let error = wrap
            .to_policy()
            .expect_err("a grant to a missing path must fail policy construction");
        assert!(
            format!("{error:#}").contains("AGENTS.md"),
            "error should name the failing grant: {error:#}"
        );
        assert!(
            !missing.exists(),
            "policy construction must never create the granted path"
        );
    }

    /// A baseline writable path (worktree root / scratch dir) that doesn't
    /// exist must also fail: silently narrowing the sandbox would hand the
    /// command less access than the model was told it has.
    #[test]
    fn to_policy_fails_on_missing_writable_path() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let missing = temp_dir.path().join("gone");

        let wrap = SandboxWrap {
            writable_paths: vec![missing.clone()],
            ..Default::default()
        };

        wrap.to_policy()
            .expect_err("a missing writable path must fail policy construction");
        assert!(
            !missing.exists(),
            "policy construction must never create a writable path"
        );
    }

    /// Protected paths are best-effort: an uncapturable one is dropped, never
    /// fatal. That covers a missing path (`NotFound`) and one routed through a
    /// regular file (`NotADirectory`) — the latter is the synthesized `.git` of
    /// a single-file worktree (e.g. `settings.json/.git`). Unlike a writable
    /// grant, a protection can't be materialized, so failing here would only
    /// break legitimate cases; a missing `<root>/.git` of a writable root is
    /// kept from being created by `sandbox::Sandbox::new` instead (its test
    /// `a_writable_root_without_a_git_directory_cannot_get_one`). Unit-level
    /// companion to the `settings.json/.git` NixOS check.
    #[test]
    fn to_policy_skips_uncapturable_protected_paths() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let writable = temp_dir.path().join("writable");
        std::fs::create_dir(&writable).expect("create writable dir");
        let single_file_root = temp_dir.path().join("settings.json");
        std::fs::write(&single_file_root, b"{}").expect("create single-file worktree root");

        let wrap = SandboxWrap {
            writable_paths: vec![writable],
            protected_paths: vec![
                temp_dir.path().join("no-such-.git"),
                single_file_root.join(".git"),
            ],
            ..Default::default()
        };

        wrap.to_policy()
            .expect("uncapturable protected paths must be dropped, not fail the policy");
    }
}

#[cfg(test)]
mod sandboxed_spawn_tests {
    use super::*;

    /// The `.git` a sandboxed command may not write is the one on the machine
    /// the worktree is on. Spelling it with this machine's separator would name
    /// something no host has, and a protected path that cannot exist is dropped
    /// — so the repository's metadata would stay writable. Runs the same on
    /// every client, which is the point: the client's own OS must not decide.
    #[test]
    fn the_protected_git_directory_is_spelled_as_its_own_machine_spells_it() {
        assert_eq!(
            dot_git_path(Path::new("/srv/app"), PathStyle::Unix)
                .to_string_lossy()
                .as_ref(),
            "/srv/app/.git"
        );
        assert_eq!(
            dot_git_path(Path::new("/srv/app/"), PathStyle::Unix)
                .to_string_lossy()
                .as_ref(),
            "/srv/app/.git",
            "a root that already ends in a separator does not gain a second one"
        );
        assert_eq!(
            dot_git_path(Path::new("C:\\work"), PathStyle::Windows)
                .to_string_lossy()
                .as_ref(),
            "C:\\work\\.git"
        );
        // A WSL share, which is how a Windows client spells a distro's folder:
        // still a Windows path, so it keeps the Windows separator.
        assert_eq!(
            dot_git_path(
                Path::new("\\\\wsl.localhost\\Ubuntu\\srv"),
                PathStyle::Windows
            )
            .to_string_lossy()
            .as_ref(),
            "\\\\wsl.localhost\\Ubuntu\\srv\\.git"
        );
    }

    #[test]
    fn a_wrapped_command_is_routed_to_a_sandbox_or_refused() {
        let local = SandboxWrap {
            is_local: true,
            ..Default::default()
        };
        let remote = SandboxWrap {
            is_local: false,
            ..Default::default()
        };
        assert_eq!(
            terminal_spawn_route(None, false).ok(),
            Some(TerminalSpawnRoute::Unwrapped)
        );
        assert_eq!(
            terminal_spawn_route(Some(&local), false).ok(),
            Some(TerminalSpawnRoute::LocalSandbox)
        );
        assert_eq!(
            terminal_spawn_route(Some(&remote), true).ok(),
            Some(TerminalSpawnRoute::RemoteHelper)
        );
        assert!(
            terminal_spawn_route(Some(&remote), false).is_err(),
            "a remote wrap whose helper went away must not fall into this machine's sandbox"
        );
    }

    /// ADR-F49: a command this machine's sandbox wraps starts from an empty
    /// environment. The terminal hands its child everything the editor's own
    /// process has, so what a wrapped command may see has to be named: its own
    /// variables, and the few a terminal needs.
    #[test]
    fn a_sandboxed_command_starts_from_an_empty_environment() {
        let environment = HashMap::from_iter([
            ("PATH".to_string(), "/sandbox/bin".to_string()),
            ("HTTPS_PROXY".to_string(), "http://127.0.0.1:1".to_string()),
            // What the caller lays in of the terminal's own: see
            // `AcpThread::create_terminal`.
            ("TERM".to_string(), "xterm-256color".to_string()),
        ]);
        let (program, args) = start_from_an_empty_environment(
            "bwrap".to_string(),
            vec!["--die-with-parent".to_string(), "sh".to_string()],
            &environment,
        );

        assert_eq!(program, "/usr/bin/env");
        assert_eq!(args.first().map(String::as_str), Some("-i"));
        assert_eq!(
            &args[args.len() - 3..],
            ["bwrap", "--die-with-parent", "sh"]
        );
        let assignments = &args[1..args.len() - 3];
        for assignment in [
            "PATH=/sandbox/bin",
            "HTTPS_PROXY=http://127.0.0.1:1",
            "TERM=xterm-256color",
        ] {
            assert!(
                assignments.contains(&assignment.to_string()),
                "{assignments:?}"
            );
        }
        assert_eq!(
            assignments
                .iter()
                .filter(|assignment| assignment.starts_with("PATH="))
                .count(),
            1,
            "the command's own variables win over this machine's: {assignments:?}"
        );
        for assignment in assignments {
            let (name, _) = assignment
                .split_once('=')
                .unwrap_or_else(|| panic!("{assignment} is not an assignment"));
            assert!(
                HOST_ENVIRONMENT_INSIDE_SANDBOX.contains(&name) || environment.contains_key(name),
                "{name} is not one of the variables a sandboxed command may be given"
            );
        }
    }

    /// The values `flag` is given in a decoded helper argv.
    fn helper_argv_values(argv: &[String], flag: &str) -> Vec<String> {
        argv.windows(2)
            .filter_map(|window| match window {
                [name, value] if name == flag => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether a command started with `argv` can reach `host`, as the helper's
    /// proxy decides it: the command's own allowlist is the base decision and
    /// the editor-wide ceiling beside it is the outer one, and a host needs
    /// both (`linux_remote_helper::start_egress_proxy`). The ceiling is an
    /// `EgressPolicy` there rather than an `Allowlist`, over the same exact and
    /// leading-`*.` patterns.
    fn remote_command_reaches(argv: &[String], host: &str) -> bool {
        let listed = |flag: &str| {
            Allowlist::from_patterns(
                helper_argv_values(argv, flag)
                    .iter()
                    .filter_map(|value| http_proxy::HostPattern::parse(value).ok()),
            )
        };
        let granted = match helper_argv_values(argv, "--net")
            .first()
            .map(String::as_str)
        {
            Some("any") => Allowlist::any(),
            Some("hosts") => listed("--allow-host"),
            _ => Allowlist::empty(),
        };
        let ceiling = argv
            .iter()
            .any(|argument| argument == "--network-ceiling")
            .then(|| listed("--ceiling-host"));
        granted.allows(host) && ceiling.is_none_or(|ceiling| ceiling.allows(host))
    }

    /// ADR-F17: what a remote host's helper runs is capped by the editor-wide
    /// allowlist, exactly as what this machine's (or WSL's) sandbox runs is.
    /// The hosts the user's sandbox settings grant reach the helper as the
    /// command's own allowlist; the editor's allowlist travels beside them as
    /// the ceiling its proxy nests that allowlist inside. A granted host the
    /// editor does not allow is therefore unreachable, and the ceiling can
    /// only narrow what the command was granted, never widen it.
    #[test]
    fn a_remote_command_reaches_only_the_part_of_its_hosts_the_editor_allows() {
        let editor_allowlist = ["api.github.com".to_string()];
        let granted = |hosts: &[&str]| {
            SandboxNetworkAccess::Restricted(Allowlist::from_patterns(
                hosts
                    .iter()
                    .map(|host| http_proxy::HostPattern::parse(host).expect("a valid pattern")),
            ))
        };
        let policy_of = |network: SandboxNetworkAccess, ceiling: Option<&[String]>| {
            let wrap = SandboxWrap {
                writable_paths: vec![PathBuf::from("/home/user/project")],
                network,
                ..Default::default()
            };
            let helper = RelPath::from_unix_str(".zed_server/zed-remote-server-stable-1.17.2")
                .expect("a valid relative path");
            let (_, args) = build_remote_sandbox_command(
                helper,
                &wrap,
                ceiling,
                None,
                &HashMap::default(),
                &[],
                false,
                None,
                None,
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "curl https://example.com".to_string()],
            );
            let [_, payload] = args.as_slice() else {
                panic!("the argv travels as one hex-encoded argument: {args:?}");
            };
            sandbox::decode_remote_sandbox_helper_argv(payload).expect("the packed argv decodes")
        };

        let outside = policy_of(granted(&["example.com"]), Some(&editor_allowlist));
        assert!(
            !remote_command_reaches(&outside, "example.com"),
            "a host the sandbox settings grant but the editor does not allow must not be \
             reachable from a remote sandbox: {outside:?}"
        );
        assert!(
            !remote_command_reaches(&outside, "api.github.com"),
            "the ceiling only narrows what the command was granted: {outside:?}"
        );

        let allowed = policy_of(
            granted(&["example.com", "api.github.com"]),
            Some(&editor_allowlist),
        );
        assert!(remote_command_reaches(&allowed, "api.github.com"));
        assert!(!remote_command_reaches(&allowed, "example.com"));

        // `allow_all_hosts` is the degenerate case of the same rule, not a way
        // out of it: the command's half permits everything and the editor's
        // allowlist is left deciding alone.
        let any_host = policy_of(SandboxNetworkAccess::All, Some(&editor_allowlist));
        assert!(remote_command_reaches(&any_host, "api.github.com"));
        assert!(
            !remote_command_reaches(&any_host, "example.com"),
            "an any-host grant must not reach past the editor's allowlist: {any_host:?}"
        );

        // A process with no editor-wide allowlist (an embedder, a test) leaves
        // the command's own allowlist as the whole policy, as it does locally.
        let uncapped = policy_of(granted(&["example.com"]), None);
        assert!(
            !uncapped
                .iter()
                .any(|argument| argument == "--network-ceiling"),
            "{uncapped:?}"
        );
        assert!(remote_command_reaches(&uncapped, "example.com"));
    }

    /// ADR-F17: the ceiling a sandboxed command is built with is the
    /// editor-wide allowlist as it stands at that moment — read from the app
    /// by `AcpThread::create_terminal` and carried into the spawn that starts
    /// the remote host's helper. The spawn is assembled here by the very
    /// function that call uses, and the ceiling by the very reader, so only the
    /// remote project itself stands between this and the command the host runs.
    /// An allowlist that permits nothing is still a ceiling and must not read
    /// as "no ceiling".
    #[gpui::test]
    fn a_command_is_built_with_the_editors_live_allowlist_as_its_ceiling(
        cx: &mut gpui::TestAppContext,
    ) {
        let spawn_with = |ceiling: &SandboxCeiling| {
            let wrap = SandboxWrap {
                writable_paths: vec![PathBuf::from("/home/user/project")],
                // The command's own half permits everything, so what it can
                // reach is the ceiling alone.
                network: SandboxNetworkAccess::All,
                ..Default::default()
            };
            let helper = RelPath::from_unix_str(".zed_server/zed-remote-server-stable-1.17.2")
                .expect("a valid relative path");
            remote_sandboxed_spawn(
                helper,
                &wrap,
                ceiling,
                None,
                &HashMap::default(),
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "curl https://example.com".to_string()],
            )
        };
        let argv_of = |spawn: &task::SpawnInTerminal| {
            let [_, payload] = spawn.args.as_slice() else {
                panic!(
                    "the argv travels as one hex-encoded argument: {:?}",
                    spawn.args
                );
            };
            sandbox::decode_remote_sandbox_helper_argv(payload).expect("the packed argv decodes")
        };

        cx.update(|cx| {
            assert_eq!(
                SandboxCeiling::read(cx).entries(),
                None,
                "a process with no editor-wide allowlist publishes no ceiling"
            );

            let (policy, errors) = http_client::EgressPolicy::from_host_strings(["api.github.com"]);
            assert!(errors.is_empty(), "{errors:?}");
            let handle = http_client::EgressPolicyHandle::new(policy);
            cx.set_global(egress_policy::EgressAllowlistGlobal(handle.clone()));

            let ceiling = SandboxCeiling::read(cx);
            assert_eq!(
                ceiling.entries(),
                Some(["api.github.com".to_string()].as_slice())
            );
            let spawn = spawn_with(&ceiling);
            assert!(
                spawn.cwd.is_none(),
                "the helper's path is relative to the remote home and it changes directory itself"
            );
            let argv = argv_of(&spawn);
            assert!(remote_command_reaches(&argv, "api.github.com"));
            assert!(
                !remote_command_reaches(&argv, "example.com"),
                "an any-host command must not reach past the editor's allowlist: {argv:?}"
            );

            // The handle is live, so the next command is built against the
            // edited allowlist rather than the one this process started with.
            let (policy, errors) = http_client::EgressPolicy::from_host_strings(["example.com"]);
            assert!(errors.is_empty(), "{errors:?}");
            handle.set(policy);
            let argv = argv_of(&spawn_with(&SandboxCeiling::read(cx)));
            assert!(remote_command_reaches(&argv, "example.com"));
            assert!(!remote_command_reaches(&argv, "api.github.com"));

            // An allowlist that permits nothing is a ceiling that refuses
            // everything, not a missing one.
            handle.set(http_client::EgressPolicy::deny_all());
            let ceiling = SandboxCeiling::read(cx);
            assert_eq!(ceiling.entries(), Some([].as_slice()));
            let argv = argv_of(&spawn_with(&ceiling));
            assert!(
                argv.iter().any(|argument| argument == "--network-ceiling"),
                "{argv:?}"
            );
            assert!(!remote_command_reaches(&argv, "example.com"));
        });
    }

    /// ADR-F17: the same ceiling reaches WSL's helper by travelling with the
    /// command — its proxy runs inside the distribution, where the live policy
    /// object cannot follow. `create_terminal` hands what it read to
    /// `prepare_sandbox_wrap`, which builds the command's sandbox with it.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    fn a_wsl_command_carries_the_editors_allowlist_as_its_ceiling(cx: &mut gpui::TestAppContext) {
        let project = tempfile::tempdir().expect("create temp dir");
        let wrap = SandboxWrap {
            writable_paths: vec![project.path().to_path_buf()],
            is_local: true,
            ..Default::default()
        };

        cx.update(|cx| {
            let uncapped = sandbox_for_wrap(&wrap, &SandboxCeiling::read(cx))
                .expect("a sandbox for the command");
            assert_eq!(
                uncapped.editor_network_allowlist(),
                None,
                "a process with no editor-wide allowlist leaves the command's own policy alone"
            );

            let (policy, errors) = http_client::EgressPolicy::from_host_strings(["api.github.com"]);
            assert!(errors.is_empty(), "{errors:?}");
            cx.set_global(egress_policy::EgressAllowlistGlobal(
                http_client::EgressPolicyHandle::new(policy),
            ));
            let capped = sandbox_for_wrap(&wrap, &SandboxCeiling::read(cx))
                .expect("a sandbox for the command");
            assert_eq!(
                capped.editor_network_allowlist(),
                Some(["api.github.com".to_string()].as_slice())
            );
        });
    }

    #[test]
    fn remote_helper_is_started_directly_with_the_environment_inside() {
        let environment = HashMap::from_iter([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("LD_PRELOAD".to_string(), "/project/evil.so".to_string()),
            ("BAD=NAME".to_string(), "dropped".to_string()),
            ("-u".to_string(), "dropped".to_string()),
            ("1ABC".to_string(), "dropped".to_string()),
        ]);
        let wrap = SandboxWrap {
            writable_paths: vec![PathBuf::from("/home/user/project")],
            ..Default::default()
        };
        let helper = RelPath::from_unix_str(".zed_server/zed-remote-server-stable-1.17.2")
            .expect("a valid relative path");

        let (program, args) = build_remote_sandbox_command(
            helper,
            &wrap,
            None,
            Some(Path::new("/home/user/project")),
            &environment,
            &[],
            false,
            None,
            None,
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "make".to_string()],
        );

        assert_eq!(
            program, ".zed_server/zed-remote-server-stable-1.17.2",
            "the helper itself starts the command, with no shell in front of it"
        );
        let [flag, payload] = args.as_slice() else {
            panic!("the argv travels as one hex-encoded argument: {args:?}");
        };
        assert_eq!(flag, sandbox::REMOTE_SANDBOX_HELPER_HEX_ARGV_FLAG);
        assert!(
            payload
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f' | b'.')),
            "nothing a login shell could read as syntax: {payload}"
        );
        let args =
            sandbox::decode_remote_sandbox_helper_argv(payload).expect("the packed argv decodes");
        let separator = args
            .iter()
            .position(|arg| arg == "--")
            .expect("the policy ends with `--`");
        let (policy, command) = args.split_at(separator);
        assert_eq!(
            policy.first().map(String::as_str),
            Some("--remote-sandbox-helper")
        );
        let command = command.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(command, ["--", "/bin/sh", "-c", "make"]);
        let forwarded = policy
            .windows(3)
            .filter_map(|window| match window {
                [flag, name, value] if flag == "--setenv" => Some((name.as_str(), value.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            forwarded,
            [("LD_PRELOAD", "/project/evil.so"), ("PATH", "/usr/bin")],
            "the environment reaches the command through the helper, sorted, and a name \
             `setenv(3)` refuses is left out"
        );
    }

    /// ADR-F53: a session with an IDE channel asks the host's helper to open it
    /// inside the command's own network namespace, next to `--interactive` and
    /// the session id; a command without one says nothing about an IDE, so the
    /// helper builds no forward at all.
    #[test]
    fn the_ide_channel_reaches_the_remote_helper_only_when_there_is_one() {
        let wrap = SandboxWrap {
            writable_paths: vec![PathBuf::from("/home/user/project")],
            ..Default::default()
        };
        let helper = RelPath::from_unix_str(".zed_server/zed-remote-server-stable-1.17.2")
            .expect("a valid relative path");
        let policy_of = |forward: Option<&sandbox::IdeForward>| {
            let (_, args) = build_remote_sandbox_command(
                helper,
                &wrap,
                None,
                None,
                &HashMap::default(),
                &[],
                true,
                forward,
                Some("claude-1"),
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "claude".to_string()],
            );
            let [_, payload] = args.as_slice() else {
                panic!("the argv travels as one hex-encoded argument: {args:?}");
            };
            sandbox::decode_remote_sandbox_helper_argv(payload).expect("the packed argv decodes")
        };

        let forward = sandbox::IdeForward {
            inner_port: 45123,
            rendezvous_path: "/home/user/.cache/zed-jp-enhance/ide/claude-1/bridge.sock"
                .to_string(),
        };
        let with_ide = policy_of(Some(&forward));
        let tail = with_ide
            .iter()
            .position(|arg| arg == "--interactive")
            .map(|start| &with_ide[start..])
            .expect("an interactive session says so");
        assert_eq!(
            tail.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "--interactive",
                "--ide-listen",
                "45123",
                "--ide-rendezvous",
                "/home/user/.cache/zed-jp-enhance/ide/claude-1/bridge.sock",
                "--session",
                "claude-1",
                "--",
                "/bin/sh",
                "-c",
                "claude",
            ],
            "the IDE flags sit where the WSL backend puts them, before the session id"
        );

        let without_ide = policy_of(None);
        assert!(
            !without_ide
                .iter()
                .any(|arg| arg == "--ide-listen" || arg == "--ide-rendezvous"),
            "a command with no IDE channel asks for none: {without_ide:?}"
        );
    }

    #[test]
    fn settings_environment_goes_inside_a_local_sandbox() {
        let environment = HashMap::from_iter([
            ("LD_PRELOAD".to_string(), "/project/evil.so".to_string()),
            (
                "HTTP_PROXY".to_string(),
                "http://elsewhere:8080".to_string(),
            ),
            ("TMPDIR".to_string(), "/project/tmp".to_string()),
        ]);
        let (program, args) = prefix_environment(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "make".to_string()],
            &environment,
        );
        assert_eq!(program, "env");
        assert_eq!(
            args,
            ["LD_PRELOAD=/project/evil.so", "/bin/sh", "-c", "make"],
            "the sandbox's own proxy and temp directory are not overridden"
        );

        let (program, args) = prefix_environment(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "make".to_string()],
            &HashMap::default(),
        );
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, ["-c", "make"]);
    }
}
