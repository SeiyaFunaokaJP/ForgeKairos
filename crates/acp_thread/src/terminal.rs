use agent_client_protocol::schema::v1 as acp;
use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt as _, future::Shared};
use gpui::{App, AppContext, AsyncApp, Context, Entity, Task};
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
use task::Shell;
use util::{get_default_system_shell_preferring_bash, paths::PathStyle, rel_path::RelPath};

/// Request to run a terminal command inside an OS-level sandbox.
///
/// Passed to [`super::AcpThread::create_terminal`]. The actual sandboxing
/// mechanism is platform-specific (macOS Seatbelt; Linux Bubblewrap; Windows
/// via Bubblewrap inside WSL), so callers describe the *intent* with plain data
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
    /// Windows/WSL only: `(release channel, version)` of the Linux `zed` to
    /// provision inside WSL as the sandbox helper (version `latest` for dev
    /// builds). Resolved by the agent (which can read the running app's release
    /// info) and forwarded to the sandbox. `None` on other platforms, or when
    /// the release can't be determined, in which case the WSL backend falls back
    /// to running bwrap without in-sandbox bind validation.
    pub wsl_zed_release: Option<(String, String)>,
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
            // Catch-all (includes WSL/Windows messages): point at the platform
            // overview for the current OS.
            LinuxWslSandboxError::Other(_) => {
                if cfg!(target_os = "windows") {
                    "windows"
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

/// Upper bound on preparing a WSL-sandboxed command. Deliberately generous:
/// the first invocation after the WSL utility VM has shut down (or after boot)
/// has to start the VM and the distro, which routinely takes 10-30 seconds on
/// slow disks or under antivirus scanning.
#[cfg(target_os = "windows")]
pub(crate) const WSL_SANDBOX_WRAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
/// The program is `/bin/sh`, running `exec "$HOME/<helper>" "$@"`, so the helper
/// — installed at a `$HOME`-relative path — resolves regardless of which
/// directory ssh happens to `cd` into, and every policy argument reaches the
/// helper as its own argv element (no shell re-quoting). The helper itself
/// `--chdir`s into the real working directory once its jail is in place.
pub(crate) fn build_remote_sandbox_command(
    remote_binary: &RelPath,
    wrap: &SandboxWrap,
    cwd: Option<&Path>,
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

    let helper_argv = sandbox::build_remote_sandbox_helper_argv(
        &writable,
        &protected,
        &network,
        wrap.allow_fs_write,
        chdir.as_deref(),
    );

    // `$0` is a placeholder; `"$@"` forwards $1.. (the helper argv + inner
    // command) to the helper verbatim.
    let relative = remote_binary.display(PathStyle::Unix).into_owned();
    let script = format!("exec \"$HOME/{relative}\" \"$@\"");
    let mut args = vec!["-c".to_string(), script, "zed-remote-sandbox".to_string()];
    args.extend(helper_argv);
    args.push(inner_command);
    args.extend(inner_args);
    ("/bin/sh".to_string(), args)
}

/// ADR-F18: the exit code the on-host remote sandbox helper uses to signal that
/// it refused to run the command because it could not build the isolation
/// (fail-closed). Mirrors `SANDBOX_SETUP_FAILED_EXIT_CODE` in
/// `sandbox::linux_remote_helper`.
pub const REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE: u32 = 126;

/// The stderr marker the helper prints before the structured `<code>: <detail>`
/// failure line (see `sandbox::linux_remote_helper::fail_closed`). The client
/// keys the fail-closed detection on this exact prefix so an ordinary command's
/// output can never masquerade as a sandbox-creation failure.
const REMOTE_SANDBOX_FAILURE_MARKER: &str = "zed-remote-sandbox: ";

/// Shared Japanese lead-in for every remote sandbox-creation failure
/// (GLOSSARY §5).
const REMOTE_SANDBOX_FAILURE_PREFIX: &str =
    "リモートホストでサンドボックスを作成できませんでした: ";

/// One-line, user-facing hint appended when a remote sandbox-creation failure is
/// surfaced: how to opt out of sandboxing, and the egress-control cost of doing
/// so. `allow_unsandboxed` is a settings key, so it stays untranslated
/// (GLOSSARY §1).
pub const REMOTE_SANDBOX_UNSANDBOX_HINT: &str = "サンドボックスを無効化するには設定 `allow_unsandboxed` を有効にしてください \
     (ただし外部通信の統制も外れます)。";

/// ADR-F18: recognize the fail-closed signature the remote sandbox helper emits
/// — exit code 126 *and* a `zed-remote-sandbox: <code>: <detail>` marker line at
/// the start of stderr — and return the user-facing Japanese explanation for
/// `<code>` (GLOSSARY §5).
///
/// Returns `None` for anything else, so an ordinary non-zero command exit is
/// never mistaken for a sandbox-creation failure: **both** the exact exit code
/// and the marker prefix must be present. `output` is the terminal's combined
/// stdout/stderr; only the first (marker) line is parsed. The command never ran
/// when this fires, so the caller should present the reason rather than treat
/// the exit code as the command's own result.
pub fn remote_sandbox_setup_failure_message(
    exit_code: Option<u32>,
    output: &str,
) -> Option<String> {
    if exit_code != Some(REMOTE_SANDBOX_SETUP_FAILED_EXIT_CODE) {
        return None;
    }
    let code = remote_sandbox_failure_code(output)?;
    Some(remote_sandbox_failure_message_for_code(code))
}

/// Writes [`remote_sandbox_setup_failure_message`] into a finished command's
/// output, so every reader of that output learns why the command never ran —
/// not just the callers that run the detection themselves.
///
/// The explanation goes *after* the marker line, not in front of it, because
/// the marker has to stay first for [`remote_sandbox_setup_failure_message`] to
/// keep recognizing it: a reader that does its own detection must not be
/// silently downgraded to the generic "failed with exit code 126".
///
/// Returns `output` untouched when the failure signature is absent, which is
/// every ordinary command.
pub fn annotate_sandbox_setup_failure(output: String, exit_code: Option<u32>) -> String {
    let Some(reason) = remote_sandbox_setup_failure_message(exit_code, &output) else {
        return output;
    };
    let leading_blanks = output.len() - output.trim_start().len();
    let (head, rest) = output.split_at(leading_blanks);
    let (marker_line, rest) = match rest.split_once('\n') {
        Some((marker_line, rest)) => (marker_line, rest),
        None => (rest, ""),
    };
    format!("{head}{marker_line}\n{reason}\n{REMOTE_SANDBOX_UNSANDBOX_HINT}\n{rest}")
}

/// Extract `<code>` from a `zed-remote-sandbox: <code>: <detail>` marker at the
/// start of `output` (leading blank lines tolerated). Returns `None` when the
/// marker prefix is absent or no `<code>:` follows it, so the detection cannot
/// fire on unrelated terminal output.
fn remote_sandbox_failure_code(output: &str) -> Option<&str> {
    let line = output.trim_start().lines().next()?;
    let rest = line.strip_prefix(REMOTE_SANDBOX_FAILURE_MARKER)?;
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
/// §5), prefixed with the shared "could not create the sandbox" lead-in. An
/// unrecognized code falls back to naming the raw code (`不明な理由 (<code>)`) so
/// a future helper addition still surfaces something actionable rather than
/// nothing.
fn remote_sandbox_failure_message_for_code(code: &str) -> String {
    let reason = match code {
        "slirp-missing" => "slirp4netns がリモートに配置されていません (ネットワーク隔離に必要)。",
        "userns-failed" => "ユーザー名前空間を作成できません (無効化されている可能性があります)。",
        "no-fs-backend" => "ファイル隔離の方式 (Landlock / mount) がどちらも使えません。",
        "landlock-failed" | "mount-failed" => "ファイル書込制限を適用できませんでした。",
        "protected-pin-failed" => {
            "保護対象のパス (`.git` / `~/.ssh` / `~/.bashrc` などの認証情報・シェル初期化ファイル) を\
             確認できませんでした (権限などを確認してください)。"
        }
        "home-unknown" => {
            "リモートのホームディレクトリを特定できませんでした \
             (常に保護するパスを確定できないため実行しません)。"
        }
        "proxy-failed" | "slirp-failed" | "relay-failed" => {
            "ネットワーク統制を確立できませんでした。"
        }
        "seccomp-failed" => "システムコール制限を適用できませんでした。",
        "exec-failed" | "chdir-failed" => "サンドボックス内でコマンドを起動できませんでした。",
        "bad-args" => "サンドボックスの起動引数が不正です (内部エラー)。",
        other => return format!("{REMOTE_SANDBOX_FAILURE_PREFIX}不明な理由 ({other})"),
    };
    format!("{REMOTE_SANDBOX_FAILURE_PREFIX}{reason}")
}

pub(crate) async fn prepare_sandbox_wrap(
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    sandbox_wrap: Option<SandboxWrap>,
    env: HashMap<String, String>,
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
    // Windows/WSL only: tell the sandbox which Linux `zed` to provision inside
    // WSL as its `--wsl-sandbox-helper`. A no-op (and a no-op setter) elsewhere.
    #[cfg(target_os = "windows")]
    if let Some((channel, version)) = sandbox_wrap.wsl_zed_release.clone() {
        sandbox.set_wsl_zed_release(channel, version);
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
                        // never ran. Explaining that here reaches everyone who
                        // reads this terminal's output — external ACP agents
                        // included, which previously got exit 126 and one line
                        // of English marker and had to guess.
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

pub async fn create_terminal_entity(
    command: String,
    args: &[String],
    env_vars: Vec<(String, String)>,
    cwd: Option<PathBuf>,
    project: &Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<Entity<terminal::Terminal>> {
    let mut env = if let Some(dir) = &cwd {
        project
            .update(cx, |project, cx| {
                project.environment().update(cx, |env, cx| {
                    env.directory_environment(dir.clone().into(), cx)
                })
            })
            .await
            .unwrap_or_default()
    } else {
        Default::default()
    };

    disable_pagers_through_env(&mut env);
    env.extend(env_vars);

    // Use remote shell or default system shell, as appropriate
    let shell = project
        .update(cx, |project, cx| {
            project
                .remote_client()
                .and_then(|r| r.read(cx).default_system_shell())
                .map(Shell::Program)
        })
        .unwrap_or_else(|| Shell::Program(get_default_system_shell_preferring_bash()));
    let is_windows = project.read_with(cx, |project, cx| project.path_style(cx).is_windows());
    let (task_command, task_args) = task::ShellBuilder::new(&shell, is_windows)
        .redirect_stdin_to_dev_null()
        .build(Some(command.clone()), &args);

    project
        .update(cx, |project, cx| {
            project.create_terminal_task(
                task::SpawnInTerminal {
                    command: Some(task_command),
                    args: task_args,
                    cwd,
                    env,
                    ..Default::default()
                },
                cx,
            )
        })
        .await
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
                 slirp4netns がリモートに配置されていません (ネットワーク隔離に必要)。"
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
                 slirp4netns がリモートに配置されていません (ネットワーク隔離に必要)。"
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
