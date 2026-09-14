use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
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
    /// see [`sandbox::SandboxNetPolicy::Unrestricted`], which this becomes.
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
    /// Any other failure, with a human-readable description.
    Other(String),
}

impl From<sandbox::SandboxError> for LinuxWslSandboxError {
    fn from(error: sandbox::SandboxError) -> Self {
        match error {
            sandbox::SandboxError::BwrapNotFound => Self::BwrapNotFound,
            sandbox::SandboxError::BwrapSetuidRejected => Self::SetuidRejected,
            sandbox::SandboxError::SandboxProbeFailed => Self::SandboxProbeFailed,
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
                "PATH 上に使える `bwrap` が見つかりません。エージェントのコマンドをサンドボックスで\
                 実行するには Bubblewrap をインストールしてください。"
                    .to_string()
            }
            LinuxWslSandboxError::SetuidRejected => {
                "見つかった `bwrap` は setuid-root のため、Zed は実行しません。setuid ではない \
                 Bubblewrap をインストールしてください。"
                    .to_string()
            }
            LinuxWslSandboxError::SandboxProbeFailed => {
                "`bwrap` はありますが、サンドボックスを作成できませんでした。このシステムでは\
                 非特権のユーザー名前空間が無効になっている可能性があります (Ubuntu 23.10 以降は \
                 AppArmor のプロファイルが必要です)。"
                    .to_string()
            }
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
            // Catch-all. On Windows this is what WSL environment failures arrive
            // as, and the requirements section says how to fix each of them.
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
    /// Protected paths, by contrast, are **best-effort**: we protect only the
    /// ones that exist at creation time (`capture` succeeding *is* the existence
    /// check), and silently drop the rest. Unlike a writable grant, a protection
    /// can't be materialized — you can't pin the inode of a path that isn't
    /// there — and there is an inherent, *accepted* loophole regardless: a
    /// command in a non-git directory can `git init` and write hooks into a
    /// `.git` that didn't exist when the sandbox was built. Since the protection
    /// is defeatable that way no matter what, failing sandbox creation over a
    /// currently-absent (or otherwise uncapturable) `.git` would only break
    /// legitimate cases — non-git projects, single-file worktrees whose
    /// synthesized `settings.json/.git` routes through a file — without closing
    /// the hole. So we drop and move on.
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

/// The environment-unavailable reason reported once every attempt timed out.
#[cfg(target_os = "windows")]
pub(crate) fn wsl_sandbox_timeout_message() -> String {
    format!(
        "WSL でサンドボックスを作成できませんでした: WSL が {} 秒以内に応答しませんでした \
         ({} 回試行)。WSL の起動に時間がかかっている可能性があります。しばらく待ってから再試行し、\
         改善しない場合は `wsl --shutdown` で WSL を再起動してください。",
        WSL_SANDBOX_WRAP_TIMEOUT.as_secs(),
        WSL_SANDBOX_WRAP_ATTEMPTS
    )
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
pub(crate) fn build_remote_sandbox_command(
    remote_binary: &RelPath,
    wrap: &SandboxWrap,
    cwd: Option<&Path>,
    environment: &HashMap<String, String>,
    inner_command: String,
    inner_args: Vec<String>,
) -> (String, Vec<String>) {
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
        &network,
        wrap.allow_fs_write,
        chdir.as_deref(),
    );
    // The policy ends with `--`, and the environment belongs before it.
    let separator = args.pop_if(|arg| arg == "--");
    let mut environment = environment
        .iter()
        .filter(|(name, value)| can_forward_environment_variable(name, value))
        .collect::<Vec<_>>();
    environment.sort();
    for (name, value) in environment {
        args.extend(["--setenv".to_string(), name.clone(), value.clone()]);
    }
    args.extend(separator);
    args.push(inner_command);
    args.extend(inner_args);
    // Packed as hex because on an SSH host this command line is parsed by the
    // user's login shell, whose quoting (csh, rc, ...) the client can't match.
    (
        remote_binary.display(PathStyle::Unix).into_owned(),
        sandbox::encode_remote_sandbox_helper_argv(&args).to_vec(),
    )
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
            "リモートのサンドボックスを使えなくなったため、エージェントのコマンドを実行しませんでした \
             (接続が切り替わった可能性があります)。再接続してから再試行してください。"
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
const READ_HIDING_UNAVAILABLE_NOTE: &str = "このホストでは、サンドボックス内のコマンドから認証情報 (`~/.ssh` など) の読み取りを防げません (ユーザー名前空間内の mount が拒否されました)。書き込みの保護とネットワークの隔離は有効です。";

/// Shared Japanese lead-in for every remote sandbox-creation failure
/// (GLOSSARY §5).
const REMOTE_SANDBOX_FAILURE_PREFIX: &str =
    "リモートホストでサンドボックスを作成できませんでした: ";

const WSL_SANDBOX_FAILURE_PREFIX: &str = "WSL でサンドボックスを作成できませんでした: ";

/// One-line, user-facing hint appended when a remote sandbox-creation failure is
/// surfaced: what the host needs for the sandbox helper (ADR-F18). There is no
/// way to run the command outside the sandbox instead (ADR-F46). The name is
/// kept because `terminal_tool` recognizes an already-annotated output by it.
pub const REMOTE_SANDBOX_UNSANDBOX_HINT: &str = "リモートホストには、ユーザー名前空間 \
     (`unshare -Un true` が成功すること)、Landlock (カーネル 5.13 以上) またはユーザー名前空間内の \
     mount、`~/.zed_server/slirp4netns` (接続時に自動で配置されます) が必要です。";

/// Appended to a WSL failure that updating WSL's kernel fixes.
const WSL_SANDBOX_UPDATE_HINT: &str = "`wsl --update` で WSL を更新し、`wsl --shutdown` で\
     再起動してから再試行してください。";

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

    fn prefix(self) -> &'static str {
        match self {
            SandboxHost::Remote => REMOTE_SANDBOX_FAILURE_PREFIX,
            SandboxHost::Wsl => WSL_SANDBOX_FAILURE_PREFIX,
        }
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
/// the start of stderr — and return the user-facing Japanese explanation for
/// `<code>` (GLOSSARY §5).
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
    Some(sandbox_failure_message_for_code(SandboxHost::Remote, code))
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
    let reason = sandbox_failure_message_for_code(host, code);
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
    read_hiding_warning_end(output).is_some()
}

/// Puts [`READ_HIDING_UNAVAILABLE_NOTE`] right after the helper's read-hiding
/// warning, once.
fn explain_read_hiding_warning(output: String) -> String {
    let (preamble, _) = split_sandbox_preamble(&output);
    if preamble.contains(READ_HIDING_UNAVAILABLE_NOTE) {
        return output;
    }
    let Some(warning_end) = read_hiding_warning_end(&output) else {
        return output;
    };
    let (before, after) = output.split_at(warning_end);
    let line_break = if before.ends_with('\n') { "" } else { "\n" };
    format!("{before}{line_break}{READ_HIDING_UNAVAILABLE_NOTE}\n{after}")
}

/// Where the line holding the helper's read-hiding warning ends, when the
/// warnings ahead of the output include one.
fn read_hiding_warning_end(output: &str) -> Option<usize> {
    let (preamble, _) = split_sandbox_preamble(output);
    let mut end = 0;
    for line in preamble.split_inclusive('\n') {
        end += line.len();
        let is_read_hiding = sandbox_warning(line.trim()).is_some_and(|warning| {
            warning
                .split_once(':')
                .is_some_and(|(code, _)| code.trim() == READ_HIDING_UNAVAILABLE_WARNING)
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
        } else if !explained && line == READ_HIDING_UNAVAILABLE_NOTE {
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

/// Map a helper failure `<code>` to its user-facing Japanese message (GLOSSARY
/// §5), prefixed with `host`'s "could not create the sandbox" lead-in. An
/// unrecognized code falls back to naming the raw code (`不明な理由 (<code>)`) so
/// a future helper addition still surfaces something actionable rather than
/// nothing.
fn sandbox_failure_message_for_code(host: SandboxHost, code: &str) -> String {
    let reason = match (host, code) {
        (SandboxHost::Remote, "slirp-missing") => {
            "サンドボックス用ヘルパー (slirp4netns) がリモートの `~/.zed_server/` にないため、通信を隔離できません。接続し直すと配置し直します。"
        }
        (SandboxHost::Wsl, "slirp-missing") => {
            "WSL 内のサンドボックス用ヘルパー (slirp4netns) が見つからないため、通信を隔離できません。\
             Zed を再起動すると配置し直します。"
        }
        (SandboxHost::Remote, "userns-failed") => {
            "ユーザー名前空間を作成できません (無効化されている可能性があります)。開発コンテナー \
             (Docker) では既定の seccomp プロファイルが `unshare` を拒否するため、`runArgs` に \
             `--cap-add SYS_ADMIN` (または `--security-opt seccomp=unconfined`) を、ネットワークを\
             使うコマンドのために `--device /dev/net/tun` も追加してください。"
        }
        (SandboxHost::Wsl, "userns-failed") => "WSL でユーザー名前空間を作成できません。",
        (SandboxHost::Remote, "no-fs-backend") => {
            "ファイル隔離の方式 (Landlock / mount) がどちらも使えません。"
        }
        (SandboxHost::Wsl, "no-fs-backend") => "WSL のカーネルが Landlock に対応していません。",
        (_, "landlock-failed" | "mount-failed") => "ファイル書込制限を適用できませんでした。",
        (_, "protected-pin-failed") => {
            "保護対象のパス (`.git` / `~/.ssh` / `~/.bashrc` などの認証情報・シェル初期化ファイル) を\
             確認できませんでした (権限などを確認してください)。"
        }
        (SandboxHost::Remote, "home-unknown") => {
            "リモートのホームディレクトリを特定できませんでした \
             (常に保護するパスを確定できないため実行しません)。"
        }
        (SandboxHost::Wsl, "home-unknown") => {
            "WSL のホームディレクトリを特定できませんでした \
             (常に保護するパスを確定できないため実行しません)。"
        }
        (_, "writable-redirected") => {
            "書込を許可したパスが、承認後に別の場所 (シンボリックリンクなど) を指すように\
             置き換えられています。安全のため実行しませんでした。"
        }
        (_, "proxy-failed" | "slirp-failed" | "relay-failed") => {
            "ネットワーク統制を確立できませんでした。"
        }
        (_, "seccomp-failed") => "システムコール制限を適用できませんでした。",
        (_, "caps-failed") => "コマンドの権限 (capability) を外せませんでした。",
        (_, "helper-mismatch") => {
            "サンドボックス用のヘルパー (または slirp4netns) が、この Zed に同梱のものと一致しません \
             (置き換えられたか壊れています)。安全のため実行しませんでした。Zed を再起動すると配置し直します。"
        }
        (SandboxHost::Remote, "subreaper-failed") => {
            "リモートのカーネルが子プロセスの回収 (PR_SET_CHILD_SUBREAPER) を拒否したため、\
             コマンドの終了後に残るプロセスを片付けられません。安全のため実行しませんでした。"
        }
        (SandboxHost::Wsl, "subreaper-failed") => {
            "WSL のカーネルが子プロセスの回収 (PR_SET_CHILD_SUBREAPER) を拒否したため、\
             コマンドの終了後に残るプロセスを片付けられません。安全のため実行しませんでした。"
        }
        (_, "signals-failed") => {
            "サンドボックス用ヘルパーのシグナル処理を設定できませんでした。安全のため実行しませんでした。"
        }
        (_, "ide-forward-invalid") => {
            "IDE 連携の受け渡し用フォルダー (またはソケット) が安全な状態ではありません \
             (権限が 0700 でない・所有者が違う・保護対象の外にある) ため、実行しませんでした。\
             Zed を再起動してから再試行してください。"
        }
        (SandboxHost::Remote, "pidns-failed") => {
            "リモートのカーネルが PID 名前空間の作成を拒否したため、コマンドの終了後に残るプロセスを\
             確実に止められません。安全のため実行しませんでした (`user.max_pid_namespaces=0` の設定や、\
             コンテナーの seccomp プロファイルが原因のことがあります)。"
        }
        (SandboxHost::Wsl, "pidns-failed") => {
            "WSL のカーネルが PID 名前空間の作成を拒否したため、コマンドの終了後に残るプロセスを\
             確実に止められません。安全のため実行しませんでした (`user.max_pid_namespaces=0` の設定が\
             原因のことがあります)。"
        }
        (_, "uidmap-failed") => {
            "ユーザー名前空間にユーザー ID の対応付け (uid_map / gid_map) を設定できませんでした。"
        }
        (_, "windows-home-root") => {
            "書込を許可したフォルダーが Windows のユーザーフォルダーそのものか、それを含んでいるため、\
             サンドボックスでは使えません (その中の認証情報を読めないようにしているため)。\
             プロジェクトはその下のフォルダーで開いてください。"
        }
        (
            _,
            "pipe-failed" | "fork-failed" | "wait-failed" | "handshake-failed" | "mkdtemp-failed",
        ) => {
            return format!(
                "{}サンドボックスの準備中に内部エラーが発生しました ({code})。",
                host.prefix()
            );
        }
        (_, "exec-failed" | "chdir-failed") => "サンドボックス内でコマンドを起動できませんでした。",
        (_, "bad-args") => "サンドボックスの起動引数が不正です (内部エラー)。",
        (_, other) => return format!("{}不明な理由 ({other})", host.prefix()),
    };
    format!("{}{reason}", host.prefix())
}

/// The editor-wide network allowlist's entries as they stand now, for the WSL
/// sandbox to nest a command's network inside (see
/// `sandbox::Sandbox::set_editor_network_allowlist`). `None` in a process that
/// has no editor-wide allowlist.
#[cfg(target_os = "windows")]
pub(crate) fn editor_network_allowlist(cx: &App) -> Option<Vec<String>> {
    let global = cx.try_global::<egress_policy::EgressAllowlistGlobal>()?;
    Some(
        global
            .0
            .snapshot()
            .patterns()
            .iter()
            .map(ToString::to_string)
            .collect(),
    )
}

pub(crate) async fn prepare_sandbox_wrap(
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    sandbox_wrap: Option<SandboxWrap>,
    env: HashMap<String, String>,
    #[cfg(target_os = "windows")] editor_network_allowlist: Option<Vec<String>>,
) -> anyhow::Result<(
    String,
    Vec<String>,
    HashMap<String, String>,
    Option<SandboxConfigHandle>,
)> {
    let Some(sandbox_wrap) = sandbox_wrap else {
        return Ok((program, args, env, None));
    };

    let mut sandbox =
        sandbox::Sandbox::new(sandbox_wrap.to_policy()?).map_err(anyhow::Error::new)?;
    #[cfg(target_os = "windows")]
    if let Some(entries) = editor_network_allowlist {
        sandbox.set_editor_network_allowlist(entries);
    }
    let command = sandbox::CommandAndArgs {
        program,
        args,
        env: env.into_iter().collect::<StdHashMap<_, _>>(),
        cwd,
    };
    let wrapped = sandbox.wrap(&command).await.map_err(anyhow::Error::new)?;
    Ok((
        wrapped.program,
        wrapped.args,
        wrapped.env.into_iter().collect(),
        Some(sandbox),
    ))
}

pub struct Terminal {
    id: acp::TerminalId,
    command: Entity<Markdown>,
    working_dir: Option<PathBuf>,
    terminal: Entity<terminal::Terminal>,
    started_at: Instant,
    output: Option<TerminalOutput>,
    output_byte_limit: Option<usize>,
    _output_task: Shared<Task<acp::TerminalExitStatus>>,
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

pub struct TerminalOutput {
    pub ended_at: Instant,
    pub exit_status: Option<ExitStatus>,
    pub content: String,
    pub original_content_len: usize,
    pub content_line_count: usize,
}

impl Terminal {
    pub fn new(
        id: acp::TerminalId,
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
            _output_task: cx
                .spawn(async move |this, cx| {
                    let exit_status = command_task.await;

                    this.update(cx, |this, cx| {
                        let (content, original_content_len) = this.truncated_output(cx);
                        let content_line_count = this.terminal.read(cx).total_lines();

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
                        let content = annotate_sandbox_setup_failure(
                            content,
                            exit_status
                                .map(|status| portable_pty::ExitStatus::from(status).exit_code()),
                        );

                        this.output = Some(TerminalOutput {
                            ended_at: Instant::now(),
                            exit_status,
                            content,
                            original_content_len,
                            content_line_count,
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

                    let exit_status = exit_status.map(portable_pty::ExitStatus::from);

                    acp::TerminalExitStatus::new()
                        .exit_code(exit_status.as_ref().map(|e| e.exit_code()))
                        .signal(exit_status.and_then(|e| e.signal().map(ToOwned::to_owned)))
                })
                .shared(),
        }
    }

    pub fn id(&self) -> &acp::TerminalId {
        &self.id
    }

    pub fn wait_for_exit(&self) -> Shared<Task<acp::TerminalExitStatus>> {
        self._output_task.clone()
    }

    pub fn kill(&mut self, cx: &mut App) {
        self.terminal.update(cx, |terminal, _cx| {
            terminal.kill_active_task();
        });
    }

    /// Marks this terminal as stopped by user action and then kills it.
    /// This should be called when the user explicitly clicks a Stop button.
    pub fn stop_by_user(&mut self, cx: &mut App) {
        self.user_stopped.store(true, Ordering::SeqCst);
        self.kill(cx);
    }

    /// Returns whether this terminal was stopped by explicit user action.
    pub fn was_stopped_by_user(&self) -> bool {
        self.user_stopped.load(Ordering::SeqCst)
    }

    pub fn current_output(&self, cx: &App) -> acp::TerminalOutputResponse {
        if let Some(output) = self.output.as_ref() {
            let exit_status = output.exit_status.map(portable_pty::ExitStatus::from);

            acp::TerminalOutputResponse::new(
                output.content.clone(),
                output.original_content_len > output.content.len(),
            )
            .exit_status(
                acp::TerminalExitStatus::new()
                    .exit_code(exit_status.as_ref().map(|e| e.exit_code()))
                    .signal(exit_status.and_then(|e| e.signal().map(ToOwned::to_owned))),
            )
        } else {
            let (current_content, original_len) = self.truncated_output(cx);
            let truncated = current_content.len() < original_len;
            acp::TerminalOutputResponse::new(current_content, truncated)
        }
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

    pub fn update_command_label(&self, label: &str, cx: &mut App) {
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
}

/// The directory subtrees the sandbox always grants write access to for a
/// project: its worktree roots. This is the single source of truth shared by
/// the agents' terminals (which hand these to the sandbox as
/// [`SandboxWrap::writable_paths`]) and the status UI (which lists them), so
/// the two can't drift if the set ever changes.
pub fn sandbox_worktree_writable_paths(project: &Project, cx: &App) -> Vec<PathBuf> {
    project
        .worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

/// The candidate `.git` paths the sandbox protects for a project. Locating these
/// requires Git knowledge the sandbox layer can't derive itself: a worktree's
/// `.git`, a linked worktree's common dir (which lives outside the worktree),
/// and every discovered repository's git/common dirs.
pub fn sandbox_git_dirs(project: &Project, cx: &App) -> Vec<PathBuf> {
    let mut git_dirs = Vec::new();

    for worktree in project.worktrees(cx) {
        let worktree = worktree.read(cx);
        let worktree_abs_path = worktree.abs_path();
        // Protect `<worktree>/.git` even when it doesn't exist yet, so a command
        // can't `git init` and then write to the freshly created metadata.
        //
        // We don't gate this on the worktree's scanned root being a directory:
        // that state can be stale/pending, and for a *single-file* worktree
        // (e.g. `settings.json` opened on its own) this synthesizes
        // `settings.json/.git`, which can never exist. That impossible path is
        // resolved authoritatively at capture time — the real filesystem
        // reports `NotADirectory`, and `SandboxWrap::to_policy` skips a
        // protected path that can't exist — so it never reaches enforcement.
        git_dirs.push(worktree_abs_path.join(".git"));
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
        return "設定でサンドボックスが無効になっています".to_string();
    }
    let project_kind = if project.is_via_collab() {
        "共同編集"
    } else if let Some(remote_client) = project.remote_client() {
        let remote_client = remote_client.read(cx);
        match remote_client.connection_type() {
            "wsl" => "WSL リモート",
            "docker" => "Docker",
            "podman" => "Podman",
            "ssh" if remote_client.remote_platform().os.as_str() != "linux" => "SSH (Linux 以外)",
            "ssh" => "SSH",
            _ => "リモート",
        }
    } else if !cx.has_flag::<SandboxingFeatureFlag>() {
        "サンドボックス機能が無効"
    } else {
        "この OS には未対応"
    };
    format!("このプロジェクトではサンドボックスを使えません ({project_kind})")
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
                !message.contains("不明な理由"),
                "known code {code} must not fall back to the unknown branch: {message}"
            );
        }
    }

    /// A few representative codes map to their exact GLOSSARY §5 wording, and
    /// aliased codes share one message.
    #[test]
    fn representative_codes_map_to_exact_wording() {
        assert_eq!(
            remote_sandbox_setup_failure_message(Some(126), "zed-remote-sandbox: slirp-missing: x"),
            Some(
                "リモートホストでサンドボックスを作成できませんでした: \
                 サンドボックス用ヘルパー (slirp4netns) がリモートの `~/.zed_server/` にないため、通信を隔離できません。接続し直すと配置し直します。"
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
                "リモートホストでサンドボックスを作成できませんでした: \
                 保護対象のパス (`.git` / `~/.ssh` / `~/.bashrc` などの認証情報・シェル初期化ファイル) を\
                 確認できませんでした (権限などを確認してください)。"
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
                "リモートホストでサンドボックスを作成できませんでした: \
                 リモートのホームディレクトリを特定できませんでした \
                 (常に保護するパスを確定できないため実行しません)。"
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
                "リモートホストでサンドボックスを作成できませんでした: 不明な理由 (brand-new-code)"
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
                "リモートホストでサンドボックスを作成できませんでした: \
                 サンドボックス用ヘルパー (slirp4netns) がリモートの `~/.zed_server/` にないため、通信を隔離できません。接続し直すと配置し直します。"
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
            assert!(!reason.contains("不明な理由"), "{code}: {reason}");
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
                    "{REMOTE_SANDBOX_FAILURE_PREFIX}サンドボックスの準備中に内部エラーが発生しました ({code})。"
                ))
            );
        }
        let unknown = annotate_sandbox_setup_failure(
            "zed-wsl-sandbox: brand-new-code: x\n".to_string(),
            Some(126),
        );
        assert!(
            unknown.contains(&format!(
                "{WSL_SANDBOX_FAILURE_PREFIX}不明な理由 (brand-new-code)"
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
        assert!(reason.contains("システムコール制限"), "{reason}");

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
    /// grant, a protection can't be materialized, and `.git` protection has an
    /// inherent accepted loophole (`git init`), so failing here would only break
    /// legitimate cases. Unit-level companion to the `settings.json/.git` NixOS
    /// check.
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
            Some(Path::new("/home/user/project")),
            &environment,
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
