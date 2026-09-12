//! Windows sandbox integration via WSL (ADR-F46).
//!
//! Sandboxed Windows terminal commands run inside WSL under the same helper that
//! isolates the agent's commands on an SSH host (ADR-F18, `linux_remote_helper`):
//! the Linux `remote_server` Zed ships for SSH remotes, run as
//! `--remote-sandbox-helper --wsl …`. It needs nothing from the distro beyond a
//! WSL 2 kernel — no `bwrap`, no packages, no network: the helper and
//! `slirp4netns` are copied from Zed's own installation into the distro, once
//! per build (see [`ensure_sandbox_helper`]). Inside WSL the helper puts the
//! command in a user + network namespace whose only way out is the helper's own
//! host-allowlisting egress proxy, confines writes with Landlock (protected
//! paths get read-only binds), and blocks `AF_UNIX` / `AF_VSOCK` sockets with
//! seccomp — which is also what keeps the command away from WSL's Windows
//! interop socket.
//!
//! Projects may be opened either from native Windows paths (`C:\Users\...`) or
//! WSL UNC paths (`\\wsl.localhost\Ubuntu\home\...`). Native drive-letter paths
//! are translated into the distro's filesystem view with `wslpath` (falling back
//! to the conventional `/mnt/<drive>/...` mapping if that fails) and use the
//! user's default WSL distro unless a WSL UNC path in the request pins a
//! specific distro.
//!
//! Errors fall into two classes the agent treats differently:
//!
//! * **Environment unavailable** — WSL missing, a missing or WSL 1 distro, WSL
//!   failing to start, the bundled helper missing or not installable, a kernel
//!   without Landlock or user namespaces, or a WSL stdout protocol breaking
//!   down. These are returned as a [`WslSandboxUnavailable`] carrying a Japanese
//!   explanation of the fix (its `Display` adds
//!   [`WSL_SANDBOX_UNAVAILABLE_PREFIX`](crate::WSL_SANDBOX_UNAVAILABLE_PREFIX)),
//!   so the agent recognizes them *by type* and asks the user to fix the
//!   environment and retry, rather than matching on message text.
//! * **Bad request** — a specific path that doesn't exist or can't be mapped
//!   into WSL, or a request mixing distros. These are ordinary `anyhow` errors
//!   *without* [`WslSandboxUnavailable`], and are reported back to the model,
//!   which can fix the request and retry.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, bail, ensure};
use futures::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use sha2::{Digest as _, Sha256};
use smol::process::{Command, Stdio};

use crate::protected_paths::WSL_SANDBOX_HELPER_HOME_RELATIVE;
use crate::{RemoteSandboxNetwork, WSL_PROBE_RESULT_PREFIX, WSL_SANDBOX_UNAVAILABLE_PREFIX};

/// Per-command relaxations of the WSL sandbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SandboxPermissions {
    /// Outbound access, always through the helper's egress proxy inside WSL.
    pub(crate) network: RemoteSandboxNetwork,
    /// The editor-wide allowlist that proxy nests `network` inside; `None`
    /// when none is in force.
    pub(crate) network_ceiling: Option<Vec<String>>,
    pub(crate) allow_fs_write: bool,
}

pub(crate) struct WritablePath {
    pub(crate) requested: PathBuf,
    pub(crate) canonical: PathBuf,
}

/// Environment variables through which a process inside WSL finds the Windows
/// interop socket. The helper's seccomp filter already makes that socket
/// unusable; removing them as well stops a command from even trying.
const WSL_INTEROP_VARIABLES: [&str; 2] = ["WSL_INTEROP", "WSLENV"];

/// The two files of an installed helper; the helper finds `slirp4netns` next to
/// its own executable. [`CHECK_SCRIPT`] spells both names out as well.
const HELPER_BINARY_NAME: &str = "zed-remote-server";
const SLIRP_BINARY_NAME: &str = "slirp4netns";

/// How long a verified helper installation is trusted before it is checked
/// again. Checking refreshes the installation's timestamp, which is what keeps
/// another build's clean-up (of anything unused for a day, see
/// [`FINALIZE_SCRIPT`]) away from it while this Zed runs.
const HELPER_RECHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

const COPY_BUFFER_SIZE: usize = 256 * 1024;

/// Prefix of [`CHECK_SCRIPT`]'s result line, picked out of any other output.
const HELPER_RESULT_PREFIX: &str = "zed-wsl-helper:";
/// Prefix of [`STAGE_SCRIPT`]'s result line.
const STAGE_RESULT_PREFIX: &str = "zed-wsl-install:";

/// Reports whether this build's helper is installed in the distro. `$1` is the
/// install key ([`helper_install_key`]), `$2` the install root relative to
/// `$HOME`, `$3` and `$4` the SHA-256 of the server and of `slirp4netns` as this
/// build installs them; all arrive as argv, never interpolated. An installation
/// counts only when the marker [`FINALIZE_SCRIPT`] writes last exists and both
/// files still hash to those digests — a replacement can match a size.
const CHECK_SCRIPT: &str = r#"
set -u
dir="$HOME/$2/sandbox/$1"
if [ -f "$dir/installed" ] && [ -x "$dir/zed-remote-server" ] && [ -x "$dir/slirp4netns" ]; then
    if ! command -v sha256sum >/dev/null 2>&1; then
        printf 'zed-wsl-helper: unverifiable %s\n' "$dir"
        exit 0
    fi
    server=$(sha256sum < "$dir/zed-remote-server")
    slirp=$(sha256sum < "$dir/slirp4netns")
    if [ "${server%% *}" = "$3" ] && [ "${slirp%% *}" = "$4" ]; then
        touch "$dir"
        printf 'zed-wsl-helper: ready %s\n' "$dir/zed-remote-server"
        exit 0
    fi
fi
printf 'zed-wsl-helper: missing %s\n' "$dir"
"#;

/// Starts an installed helper only if it is still exactly what this build
/// installed. The file is opened once, hashed through that descriptor and then
/// executed from it (`/proc/self/fd/3`), so nothing can swap it in between; the
/// helper closes the descriptor again. `$1` is the helper and `$2` its SHA-256;
/// the helper's own arguments follow. A mismatch fails the way the helper
/// itself fails closed (exit 126 and a `zed-wsl-sandbox:` line first on
/// stderr), so it is reported the same way.
const VERIFIED_EXEC_SCRIPT: &str = r#"
set -u
helper=$1
expected=$2
shift 2
fail() {
    printf 'zed-wsl-sandbox: helper-mismatch: %s\n' "$1" >&2
    exit 126
}
[ -r "$helper" ] || fail "cannot read $helper"
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is not available"
exec 3<"$helper"
actual=$(sha256sum <&3) || fail "cannot hash $helper"
[ "${actual%% *}" = "$expected" ] || fail "$helper does not match this build"
exec /proc/self/fd/3 "$@"
"#;

/// Copies one file into the install directory: stdin goes to a fresh temporary
/// file there, whose size, SHA-256 (`-` without `sha256sum`) and path are
/// reported for [`stage_file`] to check against what it sent. `$1` is the key,
/// `$2` the install root, `$3` the file's final name.
const STAGE_SCRIPT: &str = r#"
set -eu
dir="$HOME/$2/sandbox/$1"
mkdir -p "$dir"
staged=$(mktemp "$dir/.$3.XXXXXX")
if ! cat > "$staged"; then
    rm -f "$staged"
    exit 1
fi
size=$(wc -c < "$staged" | tr -d ' ')
if command -v sha256sum >/dev/null 2>&1; then
    sum=$(sha256sum < "$staged")
    sum=${sum%% *}
else
    sum=-
fi
printf 'zed-wsl-install: %s %s %s\n' "$size" "$sum" "$staged"
"#;

/// Moves the verified files into place and writes the marker [`CHECK_SCRIPT`]
/// looks for — last, so a half-finished installation never counts. Then
/// removes what nobody needs any more: other builds' directories unused for a
/// day (a running Zed refreshes its own at least every
/// [`HELPER_RECHECK_INTERVAL`]) and temporary files an interrupted copy left.
/// `$1` is the key, `$2` the install root, then a `staged-path name size`
/// triple per file.
const FINALIZE_SCRIPT: &str = r#"
set -eu
key=$1
root="$HOME/$2/sandbox"
dir="$root/$key"
shift 2
: > "$dir/installed.new"
while [ "$#" -ge 3 ]; do
    chmod 755 "$1"
    mv -f "$1" "$dir/$2"
    printf '%s %s\n' "$2" "$3" >> "$dir/installed.new"
    shift 3
done
mv -f "$dir/installed.new" "$dir/installed"
touch "$dir"
if command -v find >/dev/null 2>&1; then
    find "$root" -mindepth 1 -maxdepth 1 -type d ! -name "$key" -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
    find "$dir" -maxdepth 1 -type f -name '.*' -mmin +60 -exec rm -f {} + 2>/dev/null || true
fi
"#;

/// Marks a failure of the Windows WSL sandboxing *environment* (see the module
/// docs). The message is the Japanese explanation shown to the user, telling
/// them what to fix. Returned as the root of the `anyhow::Error` so callers
/// classify it by type ([`anyhow::Error::downcast_ref`]) instead of by matching
/// message text. Per-request failures (a missing writable path, paths mixing
/// distros) are ordinary `anyhow` errors *without* this type.
#[derive(Debug, Clone)]
pub struct WslSandboxUnavailable(String);

impl WslSandboxUnavailable {
    /// Build an environment-unavailable error from a user-facing reason
    /// (without the [`WSL_SANDBOX_UNAVAILABLE_PREFIX`], which `Display` adds).
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The reason, without the leading [`WSL_SANDBOX_UNAVAILABLE_PREFIX`].
    #[cfg(test)]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WslSandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{WSL_SANDBOX_UNAVAILABLE_PREFIX}: {}", self.0)
    }
}

impl std::error::Error for WslSandboxUnavailable {}

/// Shorthand for an [`anyhow::Error`] wrapping a [`WslSandboxUnavailable`].
fn unavailable(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(WslSandboxUnavailable::new(message))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WslPath {
    distro: Option<String>,
    path: String,
}

/// A path mapped for use inside WSL.
///
/// WSL UNC and WSL-absolute paths can be mapped structurally up front. Native
/// drive-letter paths depend on the distro's automount configuration
/// (`/etc/wsl.conf` can move the `/mnt` root), so they are translated with
/// `wslpath` inside the distro — but a distro can only be chosen after every
/// path has been parsed (WSL UNC paths pin one), hence this two-stage shape:
/// parse structurally first, then resolve native paths via [`resolve_paths`]
/// once the distro is known.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum PathMapping {
    Wsl(WslPath),
    NativeDrive {
        /// The `\\?\`-stripped, forward-slashed form that `wslpath -u`
        /// accepts (`wslpath` is a Linux binary and doesn't understand
        /// backslash separators).
        windows_path: String,
        /// The conventional `/mnt/<drive>/...` mapping, used when `wslpath`
        /// translation fails.
        fallback: WslPath,
    },
}

impl PathMapping {
    fn distro(&self) -> Option<&str> {
        match self {
            PathMapping::Wsl(path) => path.distro.as_deref(),
            PathMapping::NativeDrive { .. } => None,
        }
    }
}

/// The result of resolving a write grant inside WSL: its canonical target, plus
/// whether that target lives on a Windows-hosted (DrvFs) filesystem whose
/// sandbox-integrity guarantees are weaker than the distro's native filesystem.
pub struct ResolvedGrant {
    pub canonical: PathBuf,
    pub on_windows_fs: bool,
}

/// Whether `path` names a location on a Windows drive (reached inside WSL via
/// DrvFs), as opposed to a WSL-native (ext4) path. This is a cheap *structural*
/// classification of the path shape (a drive-letter/verbatim path maps to a
/// `NativeDrive`; WSL UNC and Linux-absolute paths are distro-native). It is
/// used to decide whether to warn about the weaker DrvFs sandbox guarantees for
/// default project paths without a WSL round-trip. For explicit grants the
/// authoritative filesystem class comes from the in-WSL resolver instead (see
/// [`resolve_canonical_for_grant`]).
pub fn path_is_on_windows_drive(path: &Path) -> bool {
    matches!(
        path_to_wsl_allowing_missing(path),
        Ok(PathMapping::NativeDrive { .. })
    )
}

/// Where WSL registers each distro of the current user.
const LXSS_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Lxss";

/// Whether WSL is structurally present: at least one distro is registered
/// under [`LXSS_KEY`]. A cheap registry read (no `wsl.exe` spawn), suitable for
/// UI gating decisions like suppressing sandbox-integrity warnings on machines
/// where no WSL sandbox could ever be constructed. It is *not* an availability
/// guarantee: a registered distro can still fail the real checks (WSL 1, a
/// kernel without Landlock, an unhealthy WSL service, …), so enforcement paths
/// must keep relying on [`wrap_invocation`]'s in-WSL round-trips.
pub fn wsl_distro_registered() -> bool {
    windows_registry::CURRENT_USER
        .open(LXSS_KEY)
        .and_then(|lxss_key| {
            Ok(lxss_key.keys()?.any(|key| {
                lxss_key
                    .open(&key)
                    .and_then(|distro| distro.get_string("DistributionName"))
                    .is_ok()
            }))
        })
        .unwrap_or(false)
}

/// One distro as registered under [`LXSS_KEY`].
#[derive(Clone, Debug)]
struct RegisteredDistro {
    id: String,
    name: String,
    /// `1` for WSL 1, `2` for WSL 2; absent on some registrations.
    version: Option<u32>,
}

/// What the registry says about the distro a command would run in, known
/// before any `wsl.exe` is started.
#[derive(Clone, Debug, Eq, PartialEq)]
enum DistroRegistration {
    /// WSL itself does not appear to be installed.
    NotInstalled,
    /// WSL is installed, but no distro is registered.
    NoDistro,
    /// Distros are registered, but none of them is the default.
    NoDefault,
    /// A WSL UNC path named a distro that is not registered.
    Missing(String),
    /// The distro runs under WSL 1, which has no Linux kernel of its own.
    Wsl1(String),
    Usable,
}

fn read_distro_registration(distro: Option<&str>) -> DistroRegistration {
    let Ok(lxss) = windows_registry::CURRENT_USER.open(LXSS_KEY) else {
        return if wsl_installed() {
            DistroRegistration::NoDistro
        } else {
            DistroRegistration::NotInstalled
        };
    };
    let distros: Vec<RegisteredDistro> = match lxss.keys() {
        Ok(keys) => keys
            .filter_map(|id| {
                let key = lxss.open(&id).ok()?;
                let name = key.get_string("DistributionName").ok()?;
                let version = key.get_u32("Version").ok();
                Some(RegisteredDistro { id, name, version })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    if distros.is_empty() && !wsl_installed() {
        return DistroRegistration::NotInstalled;
    }
    let default_id = lxss.get_string("DefaultDistribution").ok();
    classify_registration(&distros, default_id.as_deref(), distro)
}

/// Whether the WSL platform is installed, which the distro list alone can't
/// tell apart from "installed, no distro yet": the Store / MSI package lives
/// under Program Files, the older optional component under `System32\lxss`.
fn wsl_installed() -> bool {
    let program_files = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from);
    program_files.is_some_and(|directory| directory.join("WSL").join("wsl.exe").is_file())
        || windows_directory().join("System32").join("lxss").is_dir()
}

fn classify_registration(
    distros: &[RegisteredDistro],
    default_id: Option<&str>,
    requested: Option<&str>,
) -> DistroRegistration {
    if distros.is_empty() {
        return DistroRegistration::NoDistro;
    }
    let distro = match requested {
        Some(name) => match distros
            .iter()
            .find(|distro| distro.name.eq_ignore_ascii_case(name))
        {
            Some(distro) => distro,
            None => return DistroRegistration::Missing(name.to_string()),
        },
        None => match default_id.and_then(|id| {
            distros
                .iter()
                .find(|distro| distro.id.eq_ignore_ascii_case(id))
        }) {
            Some(distro) => distro,
            None => return DistroRegistration::NoDefault,
        },
    };
    if distro.version == Some(1) {
        DistroRegistration::Wsl1(distro.name.clone())
    } else {
        DistroRegistration::Usable
    }
}

fn registration_message(registration: &DistroRegistration) -> Option<String> {
    match registration {
        DistroRegistration::Usable => None,
        DistroRegistration::NotInstalled => Some(
            "WSL がインストールされていません。Windows ではエージェントのコマンドを WSL 2 の中の\
             サンドボックスで実行します。管理者として PowerShell を開いて `wsl --install` を実行し、\
             PC を再起動してください。"
                .to_string(),
        ),
        DistroRegistration::NoDistro => Some(
            "WSL のディストリビューションが登録されていません。`wsl --install -d Ubuntu` などで\
             ディストリビューションを追加してください (種類は問いません。WSL 2 で動いている必要があります)。"
                .to_string(),
        ),
        DistroRegistration::NoDefault => Some(
            "既定の WSL ディストリビューションが設定されていません。`wsl --list --verbose` で一覧を\
             確認し、`wsl --set-default <名前>` で WSL 2 のディストリビューションを既定にしてください。"
                .to_string(),
        ),
        DistroRegistration::Missing(name) => Some(format!(
            "WSL ディストリビューション `{name}` が登録されていません。`wsl --list --verbose` で\
             名前を確認してください。"
        )),
        DistroRegistration::Wsl1(name) => Some(format!(
            "WSL ディストリビューション `{name}` は WSL 1 で動いています。サンドボックスには WSL 2 が\
             必要です。`wsl --set-version {name} 2` で WSL 2 に変換するか、WSL 2 のディストリビューションを\
             `wsl --set-default <名前>` で既定にしてください。"
        )),
    }
}

fn ensure_distro_registered(distro: Option<&str>) -> Result<()> {
    match registration_message(&read_distro_registration(distro)) {
        Some(problem) => Err(unavailable(problem)),
        None => Ok(()),
    }
}

/// Resolve a requested write grant to the path we persist for it, in
/// **platform-native** form: a Windows drive path stays `C:\...` (NTFS) and a
/// WSL path stays a Linux-absolute `/...`. In both cases the path is resolved to
/// its symlink/junction-free canonical target (so a granted junction or symlink
/// is followed and shown/stored as its real target), and its backing filesystem
/// is classified.
///
/// Crucially, *each side canonicalizes what it is authoritative about*:
///
/// * A **Windows drive path** is canonicalized on the **Windows host**. A
///   junction (or symlink) on a Windows drive is a Windows filesystem object;
///   WSL's DrvFs surfaces it as a plain symlink but cannot reliably resolve it.
///   `std::fs::canonicalize` follows the junction/symlink chain and confirms
///   the target exists.
/// * A **WSL path** is canonicalized inside **WSL** by the sandbox helper
///   (`--wsl-sandbox-resolve-path`) — Windows can't follow ext4 symlinks.
///
/// Either way the helper re-verifies the persisted canonical when the command
/// runs (a writable root must still be exactly that path), so a component
/// swapped for a symlink/junction after approval fails closed.
///
/// There is no distro to pin — a sandboxed command runs in exactly one distro
/// (the project's or WSL's default), so a bare Linux path is unambiguous.
pub async fn resolve_canonical_for_grant(requested: PathBuf) -> Result<ResolvedGrant> {
    let path_string = requested.to_string_lossy();

    // A Windows drive grant (NTFS) is resolved on the Windows host, which is
    // authoritative about junctions and symlinks (see the doc comment above).
    if parse_native_drive_path(&path_string).is_ok() {
        let canonical = smol::fs::canonicalize(&requested).await.with_context(|| {
            format!(
                "failed to resolve writable path `{}` on the Windows host",
                requested.display()
            )
        })?;
        // `canonicalize` yields a `\\?\` verbatim path; store the plain native
        // spelling so a non-junction grant compares equal to what was requested.
        // A junction resolving off the local drives (onto a UNC/WSL path) is
        // rejected — only a Windows drive path maps to a DrvFs path.
        let canonical = strip_windows_verbatim_prefix(canonical);
        ensure!(
            parse_native_drive_path(&canonical.to_string_lossy()).is_ok(),
            "cannot grant sandbox write access to `{}`: it resolves to `{}`, \
             which is not a Windows drive path",
            requested.display(),
            canonical.display()
        );
        return Ok(ResolvedGrant {
            canonical,
            on_windows_fs: true,
        });
    }

    // Otherwise the grant must be a Linux-absolute WSL path, which only WSL can
    // canonicalize. `\\wsl.localhost\...` and other shapes are rejected.
    let wsl_path = parse_wsl_absolute_path(&path_string).map_err(|_| {
        anyhow::anyhow!(
            "cannot grant sandbox write access to `{}`: only Windows drive paths \
             (`C:\\...`) and WSL absolute paths (`/...`) are supported",
            requested.display()
        )
    })?;
    let mapping = PathMapping::Wsl(wsl_path);

    ensure_distro_registered(None)?;
    let wsl_exe = wsl_exe_path();
    if !wsl_exe.is_file() {
        return Err(unavailable(
            registration_message(&DistroRegistration::NotInstalled).unwrap_or_default(),
        ));
    }

    let translated = resolve_paths(&wsl_exe, None, &[(mapping, "writable path", true)])
        .await?
        .into_iter()
        .next()
        .context("bug: missing resolved writable path")?
        .context("bug: required writable path resolved as missing")?;
    let helper = ensure_sandbox_helper(&wsl_exe, None).await?;
    let mut args = helper_invocation(&helper);
    args.extend([crate::WSL_SANDBOX_RESOLVE_FLAG.to_string(), translated]);
    let output = run_wsl_command(&wsl_exe, None, &args, "書込許可の解決").await?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "failed to resolve writable path `{}`{}",
            requested.display(),
            command_failure_details(output.status.code(), &output.stderr)
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .context("WSL sandbox helper returned a non-UTF-8 canonical path")?;
    parse_canonical_grant_output(&stdout)
}

/// Strip the `\\?\` verbatim prefix that `std::fs::canonicalize` prepends to a
/// Windows drive path, leaving `\\?\UNC\...` untouched (it is not a drive path,
/// and is rejected by the caller). We persist and display the plain `C:\...`
/// spelling so a non-junction grant compares equal to the requested path.
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// `on_windows_fs` word emitted by the resolver for a Windows-hosted (DrvFs)
/// canonical target.
const RESOLVE_WINDOWS_FS: &str = "windows-fs";
/// `on_windows_fs` word emitted by the resolver for a distro-native canonical
/// target.
const RESOLVE_NATIVE_FS: &str = "native-fs";

/// Wrap a Linux process invocation so it runs inside WSL under the sandbox
/// helper.
///
/// `program` and `args` must name a Linux executable and Linux argv, not a
/// Windows executable. The caller is expected to convert the model's command
/// into a Linux shell invocation (typically `/bin/sh -c ...`) before calling
/// this function.
///
/// All writable paths, protected paths, and the cwd must be paths that can be
/// mapped into WSL. The cwd and writable paths must exist; protected paths may
/// be missing (there is nothing to protect yet). WSL UNC paths may specify a
/// distro; native drive-letter paths are translated with `wslpath` inside
/// either that distro or the default distro (falling back to
/// `/mnt/<drive>/...` if translation fails).
///
/// `env` is recreated inside the command by the helper (`--setenv`) rather than
/// being set on the `wsl.exe` process, because Windows environment variables
/// don't cross the WSL boundary unless they're listed in `WSLENV`. Variables
/// whose Windows values are meaningless or harmful inside Linux are dropped (see
/// [`is_forwardable_env_var`]).
///
/// Up to a few `wsl.exe` round-trips happen here (the helper check and probe,
/// path resolution, and on first use the helper copy), each cached, any of
/// which can take seconds when the WSL VM is cold. Run it on a background
/// executor, never on the UI thread, and bound it with a timeout — a wedged
/// `wsl.exe` (a real failure mode when the WSL service is unhealthy)
/// otherwise stalls the returned future forever. This crate deliberately has no
/// timer of its own (timers come from the caller's executor so tests stay
/// deterministic); instead it guarantees that dropping the future kills any
/// in-flight `wsl.exe` child. Parameters are owned so the returned future is
/// `Send + 'static`.
pub async fn wrap_invocation<S: BuildHasher>(
    program: String,
    args: Vec<String>,
    writable_paths: Vec<WritablePath>,
    protected_paths: Vec<PathBuf>,
    permissions: SandboxPermissions,
    cwd: Option<PathBuf>,
    env: HashMap<String, String, S>,
) -> Result<(String, Vec<String>)> {
    // Mapping failures are bad requests (a path that doesn't exist or has a
    // shape WSL can't address), not environment problems.
    let cwd_mapping =
        match &cwd {
            Some(cwd) => Some(directory_to_wsl(cwd).with_context(|| {
                format!("failed to map terminal cwd `{}` into WSL", cwd.display())
            })?),
            None => None,
        };

    let writable_requested_mappings = writable_paths
        .iter()
        .map(|path| {
            path_to_wsl(&path.requested).with_context(|| {
                format!(
                    "failed to map writable path `{}` into WSL",
                    path.requested.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let writable_mappings = writable_paths
        .iter()
        .map(|path| {
            path_to_wsl(&path.canonical).with_context(|| {
                format!(
                    "failed to map canonical writable path `{}` into WSL",
                    path.canonical.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let protected_mappings = protected_paths
        .iter()
        .map(|path| {
            path_to_wsl_allowing_missing(path).with_context(|| {
                format!("failed to map protected path `{}` into WSL", path.display())
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let distro = select_distro(
        cwd_mapping.as_ref(),
        writable_requested_mappings
            .iter()
            .chain(writable_mappings.iter())
            .chain(protected_mappings.iter()),
    )?;
    ensure_distro_registered(distro.as_deref())?;
    let wsl_exe = wsl_exe_path();
    if !wsl_exe.is_file() {
        return Err(unavailable(
            registration_message(&DistroRegistration::NotInstalled).unwrap_or_default(),
        ));
    }
    let helper = ensure_sandbox_helper(&wsl_exe, distro.as_deref()).await?;

    // Resolve all paths (translating native drive-letter paths with `wslpath`
    // now that the distro is known) in a single WSL round-trip. The cwd and
    // writable paths must exist; protected paths may be missing — the helper
    // skips those, and would itself protect the distro account's home.
    let has_cwd = cwd_mapping.is_some();
    let writable_path_count = writable_mappings.len();
    let mut mappings = Vec::with_capacity(writable_mappings.len() + protected_mappings.len() + 1);
    if let Some(mapping) = cwd_mapping {
        mappings.push((mapping, "terminal cwd", true));
    }
    mappings.extend(
        writable_mappings
            .into_iter()
            .map(|mapping| (mapping, "writable path", true)),
    );
    mappings.extend(
        protected_mappings
            .into_iter()
            .map(|mapping| (mapping, "protected path", false)),
    );
    let resolved = resolve_paths(&wsl_exe, distro.as_deref(), &mappings).await?;
    let (cwd, writable_paths, protected_paths) =
        split_resolved_paths(has_cwd, writable_path_count, resolved)?;

    let environment = forwarded_environment(&env);
    let helper_argv = crate::build_wsl_sandbox_helper_argv(
        &writable_paths,
        &protected_paths,
        &permissions.network,
        permissions.allow_fs_write,
        cwd.as_deref(),
        &crate::WslHelperOptions {
            network_ceiling: permissions.network_ceiling.as_deref(),
            slirp_sha256: helper
                .digests
                .as_ref()
                .map(|digests| digests.slirp.as_str()),
            environment: &environment,
            unset_environment: &WSL_INTEROP_VARIABLES,
        },
    );
    let wsl_args = helper_command_line(
        distro.as_deref(),
        cwd.as_deref(),
        &helper,
        helper_argv,
        program,
        args,
    );
    Ok((wsl_exe.to_string_lossy().into_owned(), wsl_args))
}

/// The `wsl.exe` arguments that run `program` under `helper`.
fn helper_command_line(
    distro: Option<&str>,
    cwd: Option<&str>,
    helper: &InstalledHelper,
    helper_argv: Vec<String>,
    program: String,
    args: Vec<String>,
) -> Vec<String> {
    let mut wsl_args = Vec::with_capacity(helper_argv.len() + args.len() + 11);
    if let Some(distro) = distro {
        wsl_args.extend(["-d".to_string(), distro.to_string()]);
    }
    if let Some(cwd) = cwd {
        wsl_args.extend(["--cd".to_string(), cwd.to_string()]);
    }
    wsl_args.extend(helper_invocation(helper));
    wsl_args.extend(helper_argv);
    wsl_args.push(program);
    wsl_args.extend(args);
    wsl_args
}

/// The `wsl.exe` arguments that start `helper`, for its own arguments to
/// follow. An installed helper is started through [`VERIFIED_EXEC_SCRIPT`]:
/// it runs outside every namespace, and the directory it lives in is inside
/// the distro account's home.
fn helper_invocation(helper: &InstalledHelper) -> Vec<String> {
    match &helper.digests {
        Some(digests) => vec![
            "--exec".to_string(),
            "/bin/sh".to_string(),
            "-c".to_string(),
            VERIFIED_EXEC_SCRIPT.to_string(),
            "zed-wsl-exec".to_string(),
            helper.path.clone(),
            digests.server.clone(),
        ],
        None => vec!["--exec".to_string(), helper.path.clone()],
    }
}

fn parse_canonical_grant_output(stdout: &str) -> Result<ResolvedGrant> {
    let mut lines = stdout.lines();
    let distro = lines
        .next()
        .context("WSL sandbox helper omitted the distro name")?;
    let canonical = lines
        .next()
        .context("WSL sandbox helper omitted the canonical path")?;
    let fs_class = lines
        .next()
        .context("WSL sandbox helper omitted the filesystem class")?;
    let on_windows_fs = match fs_class {
        RESOLVE_WINDOWS_FS => true,
        RESOLVE_NATIVE_FS => false,
        _ => bail!("WSL sandbox helper returned an invalid filesystem class: {fs_class:?}"),
    };
    ensure!(
        lines.next().is_none()
            && !distro.is_empty()
            && !distro.contains(['/', '\\'])
            && canonical.starts_with('/'),
        "WSL sandbox helper returned invalid canonical-path output: {stdout:?}"
    );
    // Store the canonical as the Linux path the helper grants. The distro is
    // pinned at enforcement from the *requested* path (see `wrap_invocation`),
    // so it is not re-encoded here: spelling the canonical as
    // `\\wsl.localhost\<distro>\...` would resurface the Windows-style path we
    // avoid and, because it names the same object as the request, show as a
    // spurious redirect in the approval UI.
    Ok(ResolvedGrant {
        canonical: PathBuf::from(canonical),
        on_windows_fs,
    })
}

fn split_resolved_paths(
    has_cwd: bool,
    writable_path_count: usize,
    resolved: Vec<Option<String>>,
) -> Result<(Option<String>, Vec<String>, Vec<String>)> {
    let mut resolved = resolved.into_iter();
    let cwd = if has_cwd {
        Some(
            resolved
                .next()
                .context("bug: missing resolved terminal cwd")?
                .context("bug: required terminal cwd resolved as missing")?,
        )
    } else {
        None
    };

    let mut writable_paths = Vec::with_capacity(writable_path_count);
    for _ in 0..writable_path_count {
        writable_paths.push(
            resolved
                .next()
                .context("bug: missing resolved writable path")?
                .context("bug: required writable path resolved as missing")?,
        );
    }

    Ok((cwd, writable_paths, resolved.flatten().collect()))
}

fn select_distro<'a>(
    cwd: Option<&PathMapping>,
    paths: impl IntoIterator<Item = &'a PathMapping>,
) -> Result<Option<String>> {
    let mut distro = cwd.and_then(|mapping| mapping.distro().map(str::to_string));
    for mapping in paths {
        let Some(path_distro) = mapping.distro() else {
            continue;
        };
        match distro.as_deref() {
            // A bad request, not an environment problem: the model (or
            // project layout) asked for paths spanning two distros, which a
            // single sandboxed command can't serve.
            Some(distro) => ensure!(
                distro == path_distro,
                "cannot sandbox a command whose paths mix WSL distros `{}` and `{}`",
                distro,
                path_distro
            ),
            None => distro = Some(path_distro.to_string()),
        }
    }
    Ok(distro)
}

// ===========================================================================
// The helper inside the distro
// ===========================================================================

/// Make sure this build's helper is installed in `distro` and that the distro
/// can run it, returning its absolute path inside the distro.
///
/// The helper is the Linux `remote_server` and `slirp4netns` shipped next to
/// `Zed.exe` (the same files `crates/remote` uploads to SSH hosts), copied into
/// `~/.cache/zed-jp-enhance/sandbox/<key>/` inside the distro — nothing is
/// downloaded, so this works offline and never runs a binary this build did not
/// ship. `<key>` is derived from the artifacts' contents, so each build (and
/// each development rebuild) has a directory of its own.
///
/// Verified installations are remembered per distro for
/// [`HELPER_RECHECK_INTERVAL`], so a steady stream of commands costs no extra
/// round-trip. Failures are not remembered, so the user can fix the
/// environment (`wsl --update`, reinstalling Zed) and retry without restarting.
///
/// `ZED_WSL_SANDBOX_HELPER` (an absolute path inside the distro to a Linux
/// `remote_server` with `slirp4netns` next to it) skips the copy, for working
/// on the helper without rebuilding the bundled artifact.
async fn ensure_sandbox_helper(wsl_exe: &Path, distro: Option<&str>) -> Result<InstalledHelper> {
    type HelperCache = HashMap<(Option<String>, String), (InstalledHelper, Instant)>;
    static CACHE: OnceLock<Mutex<HelperCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    let (install_key, artifacts) = match helper_override()? {
        Some(helper) => (helper, None),
        None => {
            let artifacts = locate_helper_artifacts()?;
            let server_digest = file_sha256(&artifacts.server).await?;
            let slirp_digest = file_sha256(&artifacts.slirp).await?;
            let digests = HelperDigests {
                server: installed_content_sha256(
                    &artifacts.server,
                    artifacts.server_is_gzip,
                    &server_digest,
                )
                .await?,
                slirp: slirp_digest.clone(),
            };
            (
                helper_install_key(&server_digest, &slirp_digest),
                Some((artifacts, digests)),
            )
        }
    };
    let cache_key = (distro.map(str::to_string), install_key.clone());
    if let Some((helper, verified_at)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&cache_key)
        && verified_at.elapsed() < HELPER_RECHECK_INTERVAL
    {
        return Ok(helper.clone());
    }

    let (helper, probe) = match &artifacts {
        None => {
            let helper = InstalledHelper {
                path: install_key.clone(),
                digests: None,
            };
            let probe = probe_helper(wsl_exe, distro, &helper).await?;
            (helper, probe)
        }
        Some((artifacts, digests)) => {
            let (path, freshly_installed) =
                install_if_needed(wsl_exe, distro, &install_key, artifacts, digests).await?;
            let helper = InstalledHelper {
                path,
                digests: Some(digests.clone()),
            };
            match probe_helper(wsl_exe, distro, &helper).await {
                Ok(probe) => (helper, probe),
                Err(error) if !freshly_installed => {
                    log::warn!(
                        "the WSL sandbox helper installed earlier does not run ({error:#}); \
                         installing it again"
                    );
                    install_helper(wsl_exe, distro, &install_key, artifacts).await?;
                    let probe = probe_helper(wsl_exe, distro, &helper).await?;
                    (helper, probe)
                }
                Err(error) => return Err(error),
            }
        }
    };
    if let Some(problem) = probe_problem(&probe, distro) {
        return Err(unavailable(problem));
    }

    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(cache_key, (helper.clone(), Instant::now()));
    Ok(helper)
}

fn helper_override() -> Result<Option<String>> {
    let Some(helper) = std::env::var_os("ZED_WSL_SANDBOX_HELPER") else {
        return Ok(None);
    };
    match helper.into_string() {
        Ok(helper) if helper.starts_with('/') => Ok(Some(helper)),
        _ => Err(unavailable(
            "環境変数 `ZED_WSL_SANDBOX_HELPER` には WSL 内の絶対パス (UTF-8) を指定してください。",
        )),
    }
}

/// This build's helper as installed in a distro.
#[derive(Clone)]
struct InstalledHelper {
    /// Absolute path inside the distro.
    path: String,
    /// What the installed files hash to, which every start checks (see
    /// [`VERIFIED_EXEC_SCRIPT`]); `None` for a `ZED_WSL_SANDBOX_HELPER`
    /// development override, which nothing vouches for.
    digests: Option<HelperDigests>,
}

/// SHA-256 digests of the installed files, in lowercase hex.
#[derive(Clone)]
struct HelperDigests {
    server: String,
    slirp: String,
}

/// The bundled helper artifacts found for this build.
struct HelperArtifacts {
    server: PathBuf,
    /// Whether `server` is the gzip-compressed form (as the installer ships it)
    /// rather than a plain local cross-build.
    server_is_gzip: bool,
    slirp: PathBuf,
}

/// File names of the artifacts for this machine's architecture: the server
/// compressed (as shipped) then plain, and `slirp4netns`. Must match
/// `script/bundle-windows.ps1`, `script/build-dev.ps1`, and what `crates/remote`
/// uploads to SSH hosts.
fn helper_artifact_names() -> ([String; 2], String) {
    let architecture = std::env::consts::ARCH;
    let server = format!("zed-remote-server-linux-{architecture}");
    (
        [format!("{server}.gz"), server],
        format!("slirp4netns-linux-{architecture}"),
    )
}

/// Where the artifacts are looked for, most preferred first: next to the
/// running executable, its `bin` directory (where the installer puts them),
/// and in debug builds the workspace's `target\debug`, where
/// `script/build-dev.ps1` always writes them — so a development binary built
/// into another target directory finds them too. `crates/remote` searches the
/// same places for its SSH uploads.
fn helper_artifact_search_dirs() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    if let Some(executable_directory) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf))
    {
        directories.push(executable_directory.join("bin"));
        directories.insert(0, executable_directory);
    }
    #[cfg(debug_assertions)]
    directories.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug"
    )));
    directories
}

fn locate_helper_artifacts() -> Result<HelperArtifacts> {
    let (server_names, slirp_name) = helper_artifact_names();
    let directories = helper_artifact_search_dirs();
    let server = directories.iter().find_map(|directory| {
        server_names
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| candidate.is_file())
    });
    let slirp = directories
        .iter()
        .map(|directory| directory.join(&slirp_name))
        .find(|candidate| candidate.is_file());
    match (server, slirp) {
        (Some(server), Some(slirp)) => Ok(HelperArtifacts {
            server_is_gzip: server
                .extension()
                .is_some_and(|extension| extension == "gz"),
            server,
            slirp,
        }),
        _ => {
            let searched = directories
                .iter()
                .map(|directory| format!("`{}`", directory.display()))
                .collect::<Vec<_>>()
                .join("、");
            Err(unavailable(format!(
                "サンドボックス用のヘルパー (`{}` と `{slirp_name}`) が Zed のインストール先に\
                 見つかりません。Zed を再インストールしてください。開発ビルドでは \
                 `script/build-dev.ps1` が `target\\debug` に用意します (探した場所: {searched})。",
                server_names[0]
            )))
        }
    }
}

/// SHA-256 of a file as stored, in hex. Cached per `(path, length, modified)`
/// for the life of the process: the artifacts only change when Zed is updated,
/// which restarts it.
async fn file_sha256(path: &Path) -> Result<String> {
    type DigestCache = HashMap<(PathBuf, u64, Option<SystemTime>), String>;
    static CACHE: OnceLock<Mutex<DigestCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    let unreadable = |error: std::io::Error| {
        unavailable(format!(
            "同梱のサンドボックス用ヘルパー `{}` を読み取れませんでした ({error})。\
             Zed を再インストールしてください。",
            path.display()
        ))
    };
    let metadata = smol::fs::metadata(path).await.map_err(unreadable)?;
    let key = (path.to_path_buf(), metadata.len(), metadata.modified().ok());
    if let Some(digest) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return Ok(digest.clone());
    }

    let mut file = smol::fs::File::open(path).await.map_err(unreadable)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer).await.map_err(unreadable)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).context("bug: read past the buffer")?);
    }
    let digest = hex_string(hasher.finalize().as_slice());
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, digest.clone());
    Ok(digest)
}

/// Names this build's helper directory inside the distro: a digest over both
/// artifacts as shipped, so no build ever runs another build's helper.
/// SHA-256 of what `path` becomes once installed: its decompressed contents
/// when `gzip`. `file_digest`, of the file as shipped, keys the cache, so each
/// build is decompressed once per process.
async fn installed_content_sha256(path: &Path, gzip: bool, file_digest: &str) -> Result<String> {
    if !gzip {
        return Ok(file_digest.to_string());
    }
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(digest) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(file_digest)
    {
        return Ok(digest.clone());
    }

    let file = smol::fs::File::open(path).await.map_err(|error| {
        unavailable(format!(
            "同梱のサンドボックス用ヘルパー `{}` を読み取れませんでした ({error})。\
             Zed を再インストールしてください。",
            path.display()
        ))
    })?;
    let mut decoder = Box::pin(async_compression::futures::bufread::GzipDecoder::new(
        futures::io::BufReader::new(file),
    ));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        let read = decoder.read(&mut buffer).await.map_err(|error| {
            unavailable(format!(
                "同梱のサンドボックス用ヘルパー `{}` が壊れています ({error})。\
                 Zed を再インストールしてください。",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).context("bug: read past the buffer")?);
    }
    let digest = hex_string(hasher.finalize().as_slice());
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(file_digest.to_string(), digest.clone());
    Ok(digest)
}

fn helper_install_key(server_digest: &str, slirp_digest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(server_digest.as_bytes());
    hasher.update(b"\n");
    hasher.update(slirp_digest.as_bytes());
    hex_string(hasher.finalize().as_slice())
        .chars()
        .take(16)
        .collect()
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The outcome of [`CHECK_SCRIPT`].
#[derive(Debug, Eq, PartialEq)]
enum HelperCheck {
    /// Installed and intact, at this absolute path.
    Ready(String),
    Missing,
    /// Installed, but the distro has no `sha256sum` to check it with.
    Unverifiable,
}

/// Installs the helper unless this build's copy is already there; returns its
/// path and whether it was installed just now.
async fn install_if_needed(
    wsl_exe: &Path,
    distro: Option<&str>,
    key: &str,
    artifacts: &HelperArtifacts,
    digests: &HelperDigests,
) -> Result<(String, bool)> {
    match check_helper(wsl_exe, distro, key, digests).await? {
        HelperCheck::Ready(helper) => return Ok((helper, false)),
        HelperCheck::Unverifiable => return Err(unverifiable_helper(distro)),
        HelperCheck::Missing => {}
    }
    install_helper(wsl_exe, distro, key, artifacts).await?;
    match check_helper(wsl_exe, distro, key, digests).await? {
        HelperCheck::Ready(helper) => Ok((helper, true)),
        HelperCheck::Unverifiable => Err(unverifiable_helper(distro)),
        HelperCheck::Missing => Err(unavailable(format!(
            "サンドボックス用のヘルパーを{}に配置しましたが、確認できませんでした。もう一度実行してください。",
            wsl_distro_label(distro)
        ))),
    }
}

fn unverifiable_helper(distro: Option<&str>) -> anyhow::Error {
    unavailable(format!(
        "{}に `sha256sum` がないため、サンドボックス用のヘルパーが置き換えられていないことを\
         確認できません。coreutils をインストールしてから再試行してください。",
        wsl_distro_label(distro)
    ))
}

async fn check_helper(
    wsl_exe: &Path,
    distro: Option<&str>,
    key: &str,
    digests: &HelperDigests,
) -> Result<HelperCheck> {
    let output = run_wsl_command(
        wsl_exe,
        distro,
        [
            "--exec",
            "sh",
            "-c",
            CHECK_SCRIPT,
            "zed-wsl-check",
            key,
            WSL_SANDBOX_HELPER_HOME_RELATIVE,
            digests.server.as_str(),
            digests.slirp.as_str(),
        ],
        "サンドボックス用ヘルパーの確認",
    )
    .await?;
    if !output.status.success() {
        return Err(unavailable(format!(
            "{}でサンドボックス用ヘルパーの状態を確認できませんでした{}。\
             `wsl --update` を実行してから再試行してください。",
            wsl_distro_label(distro),
            command_failure_details(output.status.code(), &output.stderr)
        )));
    }
    parse_check_output(&String::from_utf8_lossy(&output.stdout)).map_err(|error| {
        unavailable(format!(
            "{}でサンドボックス用ヘルパーの状態を確認できませんでした ({error:#})。",
            wsl_distro_label(distro)
        ))
    })
}

fn parse_check_output(stdout: &str) -> Result<HelperCheck> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(HELPER_RESULT_PREFIX))
        .with_context(|| format!("no helper check result in {stdout:?}"))?;
    let (state, path) = line
        .trim_start()
        .split_once(' ')
        .with_context(|| format!("malformed helper check result {line:?}"))?;
    match state {
        "ready" if path.starts_with('/') => Ok(HelperCheck::Ready(path.to_string())),
        "missing" => Ok(HelperCheck::Missing),
        "unverifiable" => Ok(HelperCheck::Unverifiable),
        _ => bail!("malformed helper check result {line:?}"),
    }
}

/// A file copied into the install directory under a temporary name, waiting
/// for [`FINALIZE_SCRIPT`].
struct StagedFile {
    name: &'static str,
    path: String,
    size: u64,
}

/// What [`STAGE_SCRIPT`] reports about the file it received.
#[derive(Debug, Eq, PartialEq)]
struct StageReport {
    size: u64,
    /// `None` when the distro has no `sha256sum`, leaving only the size check.
    sha256: Option<String>,
    path: String,
}

/// What was actually sent into WSL for one file.
struct SentFile {
    size: u64,
    sha256: String,
}

async fn install_helper(
    wsl_exe: &Path,
    distro: Option<&str>,
    key: &str,
    artifacts: &HelperArtifacts,
) -> Result<()> {
    let files = [
        (
            HELPER_BINARY_NAME,
            artifacts.server.as_path(),
            artifacts.server_is_gzip,
        ),
        (SLIRP_BINARY_NAME, artifacts.slirp.as_path(), false),
    ];
    let mut staged = Vec::with_capacity(files.len());
    for (name, source, gzip) in files {
        match stage_file(wsl_exe, distro, key, name, source, gzip).await {
            Ok(file) => staged.push(file),
            Err(error) => {
                discard_staged_files(
                    wsl_exe,
                    distro,
                    staged.iter().map(|file| file.path.as_str()),
                )
                .await;
                return Err(error);
            }
        }
    }

    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        FINALIZE_SCRIPT.to_string(),
        "zed-wsl-finalize".to_string(),
        key.to_string(),
        WSL_SANDBOX_HELPER_HOME_RELATIVE.to_string(),
    ];
    for file in &staged {
        args.extend([
            file.path.clone(),
            file.name.to_string(),
            file.size.to_string(),
        ]);
    }
    let output = run_wsl_command(wsl_exe, distro, &args, "サンドボックス用ヘルパーの配置").await?;
    if !output.status.success() {
        discard_staged_files(
            wsl_exe,
            distro,
            staged.iter().map(|file| file.path.as_str()),
        )
        .await;
        return Err(copy_failure(distro, &output));
    }
    log::info!(
        "installed the WSL sandbox helper {key} into {}",
        wsl_distro_label(distro)
    );
    Ok(())
}

/// Copy `source` (decompressed when `gzip`) into the install directory under a
/// temporary name, and check that what arrived is exactly what was sent.
async fn stage_file(
    wsl_exe: &Path,
    distro: Option<&str>,
    key: &str,
    name: &'static str,
    source: &Path,
    gzip: bool,
) -> Result<StagedFile> {
    use smol::process::windows::CommandExt as _;

    let mut command = Command::new(wsl_exe);
    if let Some(distro) = distro {
        command.args(["-d", distro]);
    }
    command
        .args([
            "--exec",
            "sh",
            "-c",
            STAGE_SCRIPT,
            "zed-wsl-stage",
            key,
            WSL_SANDBOX_HELPER_HOME_RELATIVE,
            name,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .map_err(|error| wsl_start_failure("サンドボックス用ヘルパーのコピー", &error))?;
    let stdin = child
        .stdin
        .take()
        .context("bug: wsl.exe was spawned without a stdin pipe")?;
    let (sent, output) = futures::join!(send_file(source, gzip, stdin), child.output());
    let output =
        output.map_err(|error| wsl_start_failure("サンドボックス用ヘルパーのコピー", &error))?;
    let report = parse_stage_output(&String::from_utf8_lossy(&output.stdout));

    // A failed `wsl.exe` is the better explanation of a failed send (the pipe
    // broke because the other end went away), so it is checked first.
    let failure = if !output.status.success() {
        Some(copy_failure(distro, &output))
    } else {
        match (&sent, &report) {
            (Err(_), _) => None,
            (Ok(_), Err(error)) => Some(unavailable(format!(
                "サンドボックス用のヘルパーを{}にコピーできませんでした ({error:#})。",
                wsl_distro_label(distro)
            ))),
            (Ok(sent), Ok(report)) => {
                let intact = report.size == sent.size
                    && report
                        .sha256
                        .as_deref()
                        .is_none_or(|received| received == sent.sha256);
                (!intact).then(|| {
                    unavailable(format!(
                        "{}にコピーしたサンドボックス用のヘルパー `{name}` が元のファイルと一致しませんでした \
                         (送信 {} バイト、受信 {} バイト)。もう一度実行してください。改善しない場合は \
                         WSL 内の空き容量を確認してください。",
                        wsl_distro_label(distro),
                        sent.size,
                        report.size
                    ))
                })
            }
        }
    };
    let sent = match (failure, sent) {
        (None, Ok(sent)) => sent,
        (failure, sent) => {
            if let Ok(report) = &report {
                discard_staged_files(wsl_exe, distro, [report.path.as_str()]).await;
            }
            return Err(match (failure, sent) {
                (Some(failure), _) => failure,
                (None, Err(error)) => error,
                (None, Ok(_)) => anyhow::anyhow!("bug: a staged file was discarded without cause"),
            });
        }
    };
    let report = report?;
    Ok(StagedFile {
        name,
        path: report.path,
        size: sent.size,
    })
}

/// Stream `source` (decompressed when `gzip`) into `sink`, returning the size
/// and SHA-256 of what was sent.
async fn send_file(source: &Path, gzip: bool, sink: smol::process::ChildStdin) -> Result<SentFile> {
    let file = smol::fs::File::open(source).await.map_err(|error| {
        unavailable(format!(
            "同梱のサンドボックス用ヘルパー `{}` を読み取れませんでした ({error})。\
             Zed を再インストールしてください。",
            source.display()
        ))
    })?;
    let reader = futures::io::BufReader::new(file);
    if gzip {
        let decoder = async_compression::futures::bufread::GzipDecoder::new(reader);
        pump(Box::pin(decoder), source, sink).await
    } else {
        pump(reader, source, sink).await
    }
}

async fn pump(
    mut reader: impl AsyncRead + Unpin,
    source: &Path,
    mut sink: smol::process::ChildStdin,
) -> Result<SentFile> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        // A read error here is the artifact itself (a truncated download, a
        // gzip checksum mismatch), so it names the file and the fix.
        let read = reader.read(&mut buffer).await.map_err(|error| {
            unavailable(format!(
                "同梱のサンドボックス用ヘルパー `{}` が壊れています ({error})。\
                 Zed を再インストールしてください。",
                source.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).context("bug: read past the buffer")?;
        hasher.update(chunk);
        size += read as u64;
        sink.write_all(chunk)
            .await
            .context("writing the sandbox helper into WSL")?;
    }
    sink.close()
        .await
        .context("finishing the sandbox helper copy into WSL")?;
    Ok(SentFile {
        size,
        sha256: hex_string(hasher.finalize().as_slice()),
    })
}

fn parse_stage_output(stdout: &str) -> Result<StageReport> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(STAGE_RESULT_PREFIX))
        .with_context(|| format!("no copy result in {stdout:?}"))?;
    let mut fields = line.trim_start().splitn(3, ' ');
    let (Some(size), Some(sha256), Some(path)) = (fields.next(), fields.next(), fields.next())
    else {
        bail!("malformed copy result {line:?}");
    };
    ensure!(path.starts_with('/'), "malformed copy result {line:?}");
    Ok(StageReport {
        size: size
            .parse()
            .with_context(|| format!("malformed copy result {line:?}"))?,
        sha256: (sha256 != "-").then(|| sha256.to_string()),
        path: path.to_string(),
    })
}

/// Best effort: a leftover is also swept up by the next installation.
async fn discard_staged_files<'a>(
    wsl_exe: &Path,
    distro: Option<&str>,
    paths: impl IntoIterator<Item = &'a str>,
) {
    let mut args = vec!["--exec", "rm", "-f", "--"];
    args.extend(paths);
    if args.len() == 4 {
        return;
    }
    match run_wsl_command(wsl_exe, distro, &args, "コピー途中のファイルの削除").await {
        Ok(output) if output.status.success() => {}
        Ok(output) => log::warn!(
            "could not remove a partial WSL sandbox helper copy{}",
            command_failure_details(output.status.code(), &output.stderr)
        ),
        Err(error) => log::warn!("could not remove a partial WSL sandbox helper copy: {error:#}"),
    }
}

fn copy_failure(distro: Option<&str>, output: &std::process::Output) -> anyhow::Error {
    unavailable(format!(
        "サンドボックス用のヘルパーを{}にコピーできませんでした{}。WSL 内の空き容量と、\
         ホームディレクトリへの書き込み権限を確認してください。",
        wsl_distro_label(distro),
        command_failure_details(output.status.code(), &output.stderr)
    ))
}

/// What `--wsl-sandbox-probe` reports about the distro's kernel.
#[derive(Debug, Eq, PartialEq)]
struct HelperProbe {
    landlock_abi: i32,
    /// `Err` carries the helper's one-word reason (`errno-<n>`).
    namespaces: std::result::Result<(), String>,
}

async fn probe_helper(
    wsl_exe: &Path,
    distro: Option<&str>,
    helper: &InstalledHelper,
) -> Result<HelperProbe> {
    let mut args = helper_invocation(helper);
    args.push(crate::WSL_SANDBOX_PROBE_FLAG.to_string());
    let output =
        run_wsl_command(wsl_exe, distro, &args, "サンドボックス用ヘルパーの動作確認").await?;
    parse_helper_probe(&String::from_utf8_lossy(&output.stdout)).map_err(|error| {
        unavailable(format!(
            "{}でサンドボックス用のヘルパーを実行できませんでした{} ({error:#})。\
             `wsl --update` で WSL を更新してから再試行してください。",
            wsl_distro_label(distro),
            command_failure_details(output.status.code(), &output.stderr)
        ))
    })
}

fn parse_helper_probe(stdout: &str) -> Result<HelperProbe> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(WSL_PROBE_RESULT_PREFIX))
        .with_context(|| format!("no probe result in {stdout:?}"))?;
    let mut landlock_abi = None;
    let mut namespaces = None;
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("landlock=") {
            landlock_abi = Some(
                value
                    .parse::<i32>()
                    .with_context(|| format!("malformed probe result {line:?}"))?,
            );
        } else if let Some(value) = field.strip_prefix("userns=") {
            namespaces = Some(if value == "ok" {
                Ok(())
            } else {
                Err(value.to_string())
            });
        }
    }
    Ok(HelperProbe {
        landlock_abi: landlock_abi.with_context(|| format!("malformed probe result {line:?}"))?,
        namespaces: namespaces.with_context(|| format!("malformed probe result {line:?}"))?,
    })
}

fn probe_problem(probe: &HelperProbe, distro: Option<&str>) -> Option<String> {
    if let Err(reason) = &probe.namespaces {
        return Some(format!(
            "{}でユーザー名前空間を作成できないため、サンドボックスを作れません ({reason})。\
             `wsl --update` で WSL を更新し、`wsl --shutdown` で WSL を再起動してから再試行してください \
             (WSL 2 が必要です)。",
            wsl_distro_label(distro)
        ));
    }
    if probe.landlock_abi < 1 {
        return Some(format!(
            "{}のカーネルが Landlock に対応していないため、サンドボックスを作れません。\
             `wsl --update` で WSL を更新し、`wsl --shutdown` で WSL を再起動してから再試行してください。",
            wsl_distro_label(distro)
        ));
    }
    None
}

// ===========================================================================
// Path resolution
// ===========================================================================

/// Shell script that resolves and existence-checks paths in a single WSL
/// round-trip. Arguments come in triples `(kind, path, fallback)`: kind `W`
/// is a native Windows path to translate with `wslpath -u` (falling back to
/// the precomputed `/mnt/<drive>/...` mapping when translation fails), kind
/// `L` is an already-Linux path with an empty fallback. One result line is
/// printed per triple: `<ok|fallback> <ok|missing> <resolved path>`.
const PATH_RESOLUTION_SCRIPT: &str = "\
    while [ \"$#\" -ge 3 ]; do \
        kind=$1; path=$2; fallback=$3; shift 3; translate=ok; \
        if [ \"$kind\" = W ]; then \
            resolved=$(wslpath -u \"$path\" 2>/dev/null) || { resolved=$fallback; translate=fallback; }; \
        else resolved=$path; fi; \
        exists=ok; [ -e \"$resolved\" ] || exists=missing; \
        printf '%s %s %s\\n' \"$translate\" \"$exists\" \"$resolved\"; \
    done";

/// A line of [`PATH_RESOLUTION_SCRIPT`] output, parsed.
#[derive(Debug, Eq, PartialEq)]
struct ResolvedPath {
    path: String,
    used_fallback: bool,
    exists: bool,
}

/// Resolve path mappings into final WSL paths and confirm required paths exist.
/// Native drive-letter paths are translated with `wslpath -u` inside the
/// chosen distro so its actual automount configuration is honored, falling
/// back to the structural `/mnt/<drive>` mapping when translation fails
/// (e.g. a distro without `wslpath`); a wrong fallback is still caught by
/// the existence check.
///
/// Successful resolutions are memoized per `(distro, mapping)` for the life
/// of the process, so a steady-state command whose paths have all been seen
/// before resolves with zero `wsl.exe` round-trips; at most one round-trip
/// handles all cache misses ([`resolve_uncached_paths`]). A hit reuses the
/// translation — which only changes if the distro's automount configuration
/// is edited and the distro restarted — and also skips the WSL-side
/// existence re-check. That staleness is acceptable: if a cached writable path
/// disappears mid-session the helper simply does not grant it. Optional missing
/// paths are not cached, so a protected Git path can be created and then
/// included by a later command.
///
/// Each mapping is paired with a human-readable description used in errors and
/// a flag for whether the path is required to exist. The returned paths are in
/// the same order as `mappings`; optional missing paths are returned as `None`.
async fn resolve_paths(
    wsl_exe: &Path,
    distro: Option<&str>,
    mappings: &[(PathMapping, &str, bool)],
) -> Result<Vec<Option<String>>> {
    type ResolutionCache = HashMap<Option<String>, HashMap<PathMapping, String>>;
    static CACHE: OnceLock<Mutex<ResolutionCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    let distro_key = distro.map(str::to_string);
    let mut resolved: Vec<Option<Option<String>>> = {
        let cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let per_distro = cache.get(&distro_key);
        mappings
            .iter()
            .map(|(mapping, _, _)| {
                per_distro
                    .and_then(|cached| cached.get(mapping))
                    .cloned()
                    .map(Some)
            })
            .collect()
    };

    let misses: Vec<usize> = resolved
        .iter()
        .enumerate()
        .filter(|(_, path)| path.is_none())
        .map(|(index, _)| index)
        .collect();
    if !misses.is_empty() {
        let miss_mappings: Vec<&(PathMapping, &str, bool)> = misses
            .iter()
            .filter_map(|&index| mappings.get(index))
            .collect();
        let miss_resolved = resolve_uncached_paths(wsl_exe, distro, &miss_mappings).await?;
        let mut cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let per_distro = cache.entry(distro_key).or_default();
        for ((&index, mapping), path) in misses.iter().zip(miss_mappings).zip(miss_resolved) {
            if let Some(path) = &path {
                per_distro.insert(mapping.0.clone(), path.clone());
            }
            if let Some(slot) = resolved.get_mut(index) {
                *slot = Some(path);
            }
        }
    }

    let mut paths = Vec::with_capacity(resolved.len());
    for path in resolved {
        paths.push(path.context("bug: a path mapping was left unresolved")?);
    }
    Ok(paths)
}

/// Resolve and existence-check mappings that weren't in the cache, in a
/// single `wsl.exe` round-trip. A non-login shell runs the script so profile
/// scripts can't pollute the stdout protocol.
async fn resolve_uncached_paths(
    wsl_exe: &Path,
    distro: Option<&str>,
    mappings: &[&(PathMapping, &str, bool)],
) -> Result<Vec<Option<String>>> {
    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        PATH_RESOLUTION_SCRIPT.to_string(),
        // argv[0] for the script; the path triples follow as "$@".
        "zed-resolve-paths".to_string(),
    ];
    args.extend(path_resolution_args(
        mappings.iter().map(|mapping| &mapping.0),
    ));
    let output = run_wsl_command(wsl_exe, distro, &args, "パスの解決").await?;
    if !output.status.success() {
        return Err(unavailable(format!(
            "{}でパスを解決できませんでした{}。`wsl --update` を実行してから再試行してください。",
            wsl_distro_label(distro),
            command_failure_details(output.status.code(), &output.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resolved = parse_path_resolution_output(&stdout, mappings.len()).map_err(|error| {
        unavailable(format!(
            "{}でパスを解決できませんでした ({error:#})。",
            wsl_distro_label(distro)
        ))
    })?;

    mappings
        .iter()
        .zip(resolved)
        .map(|((mapping, description, required), resolved)| {
            if resolved.used_fallback
                && let PathMapping::NativeDrive { windows_path, .. } = mapping
            {
                log::warn!(
                    "failed to translate `{windows_path}` with wslpath in {}; \
                     falling back to `{}`",
                    wsl_distro_label(distro),
                    resolved.path
                );
            }
            if !resolved.exists {
                // A bad request (the path simply isn't there), not an
                // environment problem — the model can create it or fix the path
                // and retry. Protected paths are allowed to be absent: there is
                // nothing to protect yet.
                ensure!(
                    !required,
                    "mapped {description} `{}` does not exist in {}",
                    resolved.path,
                    distro.unwrap_or("the default WSL distro")
                );
                return Ok(None);
            }
            Ok(Some(resolved.path))
        })
        .collect()
}

/// Flatten path mappings into the `(kind, path, fallback)` argument triples
/// consumed by [`PATH_RESOLUTION_SCRIPT`].
fn path_resolution_args<'a>(mappings: impl Iterator<Item = &'a PathMapping>) -> Vec<String> {
    let mut args = Vec::new();
    for mapping in mappings {
        match mapping {
            PathMapping::Wsl(path) => {
                args.extend(["L".to_string(), path.path.clone(), String::new()]);
            }
            PathMapping::NativeDrive {
                windows_path,
                fallback,
            } => {
                args.extend(["W".to_string(), windows_path.clone(), fallback.path.clone()]);
            }
        }
    }
    args
}

/// Parse [`PATH_RESOLUTION_SCRIPT`] output: one strictly-formatted line per
/// input triple. Anything else (wrong line count, unknown status words, a
/// non-absolute path) means the stdout protocol was corrupted and is an error.
fn parse_path_resolution_output(stdout: &str, expected: usize) -> Result<Vec<ResolvedPath>> {
    let lines: Vec<&str> = stdout.lines().collect();
    ensure!(
        lines.len() == expected,
        "expected {expected} result lines from the path resolution script, got {}: {stdout:?}",
        lines.len()
    );
    lines
        .into_iter()
        .map(|line| {
            let mut parts = line.splitn(3, ' ');
            let (Some(translate), Some(exists), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                bail!("malformed line from the path resolution script: {line:?}");
            };
            let used_fallback = match translate {
                "ok" => false,
                "fallback" => true,
                _ => bail!("malformed line from the path resolution script: {line:?}"),
            };
            let exists = match exists {
                "ok" => true,
                "missing" => false,
                _ => bail!("malformed line from the path resolution script: {line:?}"),
            };
            ensure!(
                path.starts_with('/'),
                "unexpected resolved path from the path resolution script: {path:?}"
            );
            Ok(ResolvedPath {
                path: path.to_string(),
                used_fallback,
                exists,
            })
        })
        .collect()
}

// ===========================================================================
// wsl.exe
// ===========================================================================

/// `CREATE_NO_WINDOW` process creation flag. `wsl.exe` is a console-subsystem
/// binary, so spawning it from a GUI process without this flag flashes a
/// console window. Defined locally because this crate doesn't depend on
/// `util` (whose command helpers normally take care of this).
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Invoke `wsl.exe` with the given args and return its raw output.
///
/// Only spawn failures become errors here; callers interpret the exit status
/// themselves. `description` names the step in Japanese for the error message.
///
/// `output()` spawns the child eagerly and the returned future owns it, so
/// with `kill_on_drop` the child can't outlive this future: a caller-side
/// timeout or cancellation that drops us also terminates a wedged `wsl.exe`
/// instead of leaking it.
async fn run_wsl_command(
    wsl_exe: &Path,
    distro: Option<&str>,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    description: &str,
) -> Result<std::process::Output> {
    use smol::process::windows::CommandExt as _;

    let mut command = Command::new(wsl_exe);
    if let Some(distro) = distro {
        command.args(["-d", distro]);
    }
    command
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);

    command
        .output()
        .await
        .map_err(|error| wsl_start_failure(description, &error))
}

fn wsl_start_failure(description: &str, error: &std::io::Error) -> anyhow::Error {
    unavailable(format!(
        "{description}のために WSL を起動できませんでした ({error})。`wsl --update` を実行するか、\
         Windows を再起動してから再試行してください。"
    ))
}

fn command_failure_details(exit_code: Option<i32>, stderr: &[u8]) -> String {
    let stderr = decode_wsl_text(stderr);
    let stderr = stderr.trim();
    let exit_status = match exit_code {
        Some(code) => format!("終了コード {code}"),
        None => "シグナルで終了".to_string(),
    };
    if stderr.is_empty() {
        format!(" ({exit_status})")
    } else {
        format!(" ({exit_status}、エラー出力: {stderr})")
    }
}

/// Decode `wsl.exe` output. What a Linux program prints arrives as UTF-8, but
/// `wsl.exe`'s own messages (an unknown distro, a WSL that fails to start —
/// exactly the ones worth showing) are UTF-16LE.
fn decode_wsl_text(bytes: &[u8]) -> String {
    let high_bytes_zero = bytes
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|byte| **byte == 0)
        .count();
    let looks_like_utf16 =
        bytes.len() >= 2 && bytes.len().is_multiple_of(2) && high_bytes_zero * 4 >= bytes.len();
    if looks_like_utf16 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .filter_map(|pair| match pair {
                [low, high] => Some(u16::from_le_bytes([*low, *high])),
                _ => None,
            })
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn wsl_distro_label(distro: Option<&str>) -> String {
    match distro {
        Some(distro) => format!("WSL ディストリビューション `{distro}`"),
        None => "既定の WSL ディストリビューション".to_string(),
    }
}

fn windows_directory() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn wsl_exe_path() -> PathBuf {
    windows_directory().join("System32").join("wsl.exe")
}

// ===========================================================================
// Environment and path mapping
// ===========================================================================

/// The part of the caller's environment to recreate inside the command, sorted
/// by name so the command line is stable. Without it the command would lose
/// `PAGER` (which stops `git` from paging into the PTY) and the rest of the
/// project environment.
fn forwarded_environment<S: BuildHasher>(
    env: &HashMap<String, String, S>,
) -> Vec<(String, String)> {
    let mut forwarded: Vec<(String, String)> = env
        .iter()
        .filter(|(name, _)| is_forwardable_env_var(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    forwarded.sort();
    forwarded
}

/// Whether an environment variable should be forwarded into the Linux sandbox.
///
/// The helper sets each one with `setenv(3)`, which rejects names that are
/// empty or contain `=`. Windows process environments include such entries —
/// most notably the per-drive current-directory pseudo-variables (`=C:`,
/// `=D:`, ...) Windows keeps in the environment block — so they are skipped.
///
/// Beyond that, many variables hold Windows-specific values that would be
/// meaningless or actively break the command inside WSL, so they are dropped
/// rather than forwarded: `PATH`/`PATHEXT` would shadow WSL's own `PATH` and
/// stop the shell from finding Linux executables, the temp-dir variables point
/// at Windows paths that don't exist in WSL (the helper provides a private
/// `TMPDIR` instead), the WSL interop variables would undermine the interop
/// block, a Windows-set `HOME` would clobber the distro's own `$HOME` and break
/// the shell, and the rest are Windows system locations, shell settings,
/// host/session identity, and CPU descriptors that are either wrong or
/// misleading inside Linux. This is a blocklist rather than an allowlist so
/// genuinely portable variables (e.g. `LANG`, `CARGO_TERM_COLOR`, `PAGER`)
/// still reach the command; it only needs to cover the variables Windows
/// populates by default. Matched case-insensitively because Windows
/// environment variable names are.
pub fn is_forwardable_env_var(name: &str) -> bool {
    if name.is_empty() || name.contains('=') {
        return false;
    }
    const BLOCKED: &[&str] = &[
        // Shadow or break the Linux process environment.
        "PATH",
        "PATHEXT",
        "TMPDIR",
        "TMP",
        "TEMP",
        // Would undermine the WSL interop block.
        "WSL_INTEROP",
        "WSLENV",
        // Windows system locations and shell, meaningless inside Linux.
        "OS",
        "COMSPEC",
        "WINDIR",
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        // When Windows has `HOME` set (e.g. for git), it's a Windows path that
        // would clobber the distro's correct Linux `$HOME` and break the shell.
        "HOME",
        "HOMEDRIVE",
        "HOMEPATH",
        "HOMESHARE",
        "USERPROFILE",
        "PUBLIC",
        "ALLUSERSPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "PROGRAMW6432",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
        "COMMONPROGRAMW6432",
        "PSMODULEPATH",
        "DRIVERDATA",
        "ONEDRIVE",
        // Windows host/session identity, misleading inside Linux.
        "COMPUTERNAME",
        "USERNAME",
        "USERDOMAIN",
        "USERDOMAIN_ROAMINGPROFILE",
        "LOGONSERVER",
        "SESSIONNAME",
        // Windows CPU descriptors.
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "PROCESSOR_ARCHITEW6432",
        "PROCESSOR_IDENTIFIER",
        "PROCESSOR_LEVEL",
        "PROCESSOR_REVISION",
    ];
    !BLOCKED
        .iter()
        .any(|blocked| name.eq_ignore_ascii_case(blocked))
}

fn directory_to_wsl(path: &Path) -> Result<PathMapping> {
    ensure!(
        path.is_dir(),
        "Windows sandboxing via WSL can only use an existing directory as cwd: {}",
        path.display()
    );
    map_path_to_wsl(path)
}

fn path_to_wsl(path: &Path) -> Result<PathMapping> {
    let path_string = path.to_string_lossy();
    if let Ok(path) = parse_wsl_absolute_path(&path_string) {
        return Ok(PathMapping::Wsl(path));
    }

    ensure!(
        path.is_dir() || path.is_file(),
        "Windows sandboxing via WSL can only grant existing files or directories: {}",
        path.display()
    );
    map_path_to_wsl(path)
}

fn path_to_wsl_allowing_missing(path: &Path) -> Result<PathMapping> {
    let path_string = path.to_string_lossy();
    if let Ok(path) = parse_wsl_absolute_path(&path_string) {
        return Ok(PathMapping::Wsl(path));
    }
    map_path_to_wsl(path)
}

fn map_path_to_wsl(path: &Path) -> Result<PathMapping> {
    let path_string = path.to_string_lossy();
    if let Ok(path) = parse_wsl_unc_path(&path_string) {
        return Ok(PathMapping::Wsl(path));
    }
    let fallback = parse_native_drive_path(&path_string)?;
    let windows_path = path_string
        .strip_prefix(r"\\?\")
        .unwrap_or(&path_string)
        .replace('\\', "/");
    Ok(PathMapping::NativeDrive {
        windows_path,
        fallback,
    })
}

fn parse_wsl_absolute_path(path: &str) -> Result<WslPath> {
    let path = path.replace('\\', "/");
    ensure!(
        path.starts_with('/') && !path.starts_with("//"),
        "path is not a WSL absolute path: {path}"
    );
    Ok(WslPath { distro: None, path })
}

fn parse_wsl_unc_path(path: &str) -> Result<WslPath> {
    let path = path.replace('/', "\\");
    let remainder = path
        .strip_prefix("\\\\wsl.localhost\\")
        .or_else(|| path.strip_prefix("\\\\wsl$\\"))
        .or_else(|| path.strip_prefix("\\\\?\\UNC\\wsl.localhost\\"))
        .or_else(|| path.strip_prefix("\\\\?\\UNC\\wsl$\\"))
        .with_context(|| format!("path is not a WSL UNC path: {path}"))?;

    let (distro, rest) = remainder
        .split_once('\\')
        .map(|(distro, rest)| (distro, Some(rest)))
        .unwrap_or((remainder, None));
    ensure!(
        !distro.is_empty(),
        "WSL UNC path is missing a distro name: {path}"
    );

    let linux_path = match rest {
        Some(rest) if !rest.is_empty() => format!("/{}", rest.replace('\\', "/")),
        _ => "/".to_string(),
    };

    Ok(WslPath {
        distro: Some(distro.to_string()),
        path: linux_path,
    })
}

fn parse_native_drive_path(path: &str) -> Result<WslPath> {
    let path = path
        .strip_prefix("\\\\?\\")
        .unwrap_or(path)
        .replace('\\', "/");
    let mut chars = path.chars();
    let Some(drive) = chars.next().filter(|drive| drive.is_ascii_alphabetic()) else {
        bail!("path is not a drive-letter Windows path: {path}");
    };
    ensure!(chars.next() == Some(':'), "path is not absolute: {path}");
    let rest = chars.as_str().trim_start_matches('/');
    let drive = drive.to_ascii_lowercase();
    let linux_path = if rest.is_empty() {
        format!("/mnt/{drive}")
    } else {
        format!("/mnt/{drive}/{rest}")
    };
    Ok(WslPath {
        distro: None,
        path: linux_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_invocation_future_is_send() {
        // Callers run `wrap_invocation` via `background_spawn`, which
        // requires a `Send` future. This fails to compile if, for example, a
        // cache `MutexGuard` is ever held across an await point.
        fn assert_send<T: Send>(_: T) {}
        assert_send(wrap_invocation(
            String::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SandboxPermissions {
                network: RemoteSandboxNetwork::Blocked,
                network_ceiling: None,
                allow_fs_write: false,
            },
            None,
            HashMap::<String, String>::new(),
        ));
    }

    #[test]
    fn helper_command_line_starts_the_verified_helper_after_wsl_options() {
        let helper = InstalledHelper {
            path: "/home/me/.cache/zed-jp-enhance/sandbox/abc/zed-remote-server".to_string(),
            digests: Some(HelperDigests {
                server: "server-digest".to_string(),
                slirp: "slirp-digest".to_string(),
            }),
        };
        let helper_argv = crate::build_wsl_sandbox_helper_argv(
            &["/mnt/c/proj".to_string()],
            &["/mnt/c/proj/.git".to_string()],
            &RemoteSandboxNetwork::Hosts(vec!["github.com".to_string()]),
            false,
            Some("/mnt/c/proj"),
            &crate::WslHelperOptions {
                network_ceiling: None,
                slirp_sha256: Some("slirp-digest"),
                environment: &[("PAGER".to_string(), String::new())],
                unset_environment: &WSL_INTEROP_VARIABLES,
            },
        );
        let args = helper_command_line(
            Some("Ubuntu"),
            Some("/mnt/c/proj"),
            &helper,
            helper_argv,
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "echo hi".to_string()],
        );
        assert_eq!(
            args,
            [
                "-d",
                "Ubuntu",
                "--cd",
                "/mnt/c/proj",
                "--exec",
                "/bin/sh",
                "-c",
                VERIFIED_EXEC_SCRIPT,
                "zed-wsl-exec",
                "/home/me/.cache/zed-jp-enhance/sandbox/abc/zed-remote-server",
                "server-digest",
                "--remote-sandbox-helper",
                "--net",
                "hosts",
                "--allow-host",
                "github.com",
                "--writable",
                "/mnt/c/proj",
                "--protected",
                "/mnt/c/proj/.git",
                "--chdir",
                "/mnt/c/proj",
                "--wsl",
                "--slirp-sha256",
                "slirp-digest",
                "--setenv",
                "PAGER",
                "",
                "--unsetenv",
                "WSL_INTEROP",
                "--unsetenv",
                "WSLENV",
                "--",
                "/bin/sh",
                "-c",
                "echo hi",
            ]
        );
    }

    /// A development override has nothing to verify against and is started
    /// directly; an installed helper only through the verifying shell, which
    /// fails the way the helper fails closed and runs what it hashed.
    #[test]
    fn only_an_installed_helper_is_started_through_the_verifier() {
        let development = InstalledHelper {
            path: "/opt/zed/remote_server".to_string(),
            digests: None,
        };
        assert_eq!(
            helper_invocation(&development),
            ["--exec", "/opt/zed/remote_server"]
        );
        assert!(VERIFIED_EXEC_SCRIPT.contains("zed-wsl-sandbox: helper-mismatch:"));
        assert!(VERIFIED_EXEC_SCRIPT.contains("exit 126"));
        assert!(VERIFIED_EXEC_SCRIPT.contains("exec /proc/self/fd/3 \"$@\""));
    }

    #[test]
    fn forwarded_environment_keeps_portable_variables_in_name_order() {
        let env = HashMap::from([
            ("PAGER".to_string(), String::new()),
            ("LANG".to_string(), "en_US.UTF-8".to_string()),
            ("CARGO_TERM_COLOR".to_string(), "always".to_string()),
        ]);
        assert_eq!(
            forwarded_environment(&env),
            [
                ("CARGO_TERM_COLOR".to_string(), "always".to_string()),
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
                ("PAGER".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn forwarded_environment_drops_windows_specific_and_invalid_names() {
        // These hold Windows paths/values that would break or be meaningless
        // inside WSL, or names `setenv(3)` rejects (the per-drive `=C:`
        // pseudo-variables). Names are matched case-insensitively, as Windows
        // env var names are.
        let env = HashMap::from([
            ("Path".to_string(), r"C:\Windows\System32".to_string()),
            ("PATHEXT".to_string(), ".COM;.EXE;.BAT".to_string()),
            (
                "TEMP".to_string(),
                r"C:\Users\me\AppData\Local\Temp".to_string(),
            ),
            ("TMPDIR".to_string(), r"C:\tmp".to_string()),
            (
                "WSL_INTEROP".to_string(),
                "/run/WSL/123_interop".to_string(),
            ),
            ("WsLeNv".to_string(), "WSL_INTEROP/u".to_string()),
            ("HOME".to_string(), r"C:\Users\me".to_string()),
            ("USERPROFILE".to_string(), r"C:\Users\me".to_string()),
            (
                "ProgramFiles(x86)".to_string(),
                r"C:\Program Files (x86)".to_string(),
            ),
            ("COMPUTERNAME".to_string(), "DESKTOP-ABC".to_string()),
            ("=C:".to_string(), r"C:\Users\me".to_string()),
            (String::new(), "value".to_string()),
            ("OK".to_string(), "value".to_string()),
        ]);
        assert_eq!(
            forwarded_environment(&env),
            [("OK".to_string(), "value".to_string())]
        );
    }

    fn registered(id: &str, name: &str, version: Option<u32>) -> RegisteredDistro {
        RegisteredDistro {
            id: id.to_string(),
            name: name.to_string(),
            version,
        }
    }

    #[test]
    fn registration_is_classified_from_the_lxss_entries() {
        let distros = [
            registered("{a}", "Ubuntu", Some(2)),
            registered("{b}", "Legacy", Some(1)),
            registered("{c}", "Old", None),
        ];
        assert_eq!(
            classify_registration(&[], None, None),
            DistroRegistration::NoDistro
        );
        assert_eq!(
            classify_registration(&distros, Some("{A}"), None),
            DistroRegistration::Usable,
            "registry ids compare case-insensitively"
        );
        assert_eq!(
            classify_registration(&distros, Some("{b}"), None),
            DistroRegistration::Wsl1("Legacy".to_string())
        );
        assert_eq!(
            classify_registration(&distros, Some("{c}"), None),
            DistroRegistration::Usable,
            "an unrecorded version is left to the probe"
        );
        assert_eq!(
            classify_registration(&distros, Some("{missing}"), None),
            DistroRegistration::NoDefault
        );
        assert_eq!(
            classify_registration(&distros, None, None),
            DistroRegistration::NoDefault
        );
        assert_eq!(
            classify_registration(&distros, Some("{a}"), Some("legacy")),
            DistroRegistration::Wsl1("Legacy".to_string()),
            "a distro pinned by a UNC path wins over the default"
        );
        assert_eq!(
            classify_registration(&distros, Some("{a}"), Some("Debian")),
            DistroRegistration::Missing("Debian".to_string())
        );
    }

    #[test]
    fn every_registration_problem_names_its_fix() {
        let cases = [
            (DistroRegistration::NotInstalled, "wsl --install"),
            (DistroRegistration::NoDistro, "wsl --install -d"),
            (DistroRegistration::NoDefault, "wsl --set-default"),
            (
                DistroRegistration::Missing("Debian".to_string()),
                "wsl --list --verbose",
            ),
            (
                DistroRegistration::Wsl1("Legacy".to_string()),
                "wsl --set-version Legacy 2",
            ),
        ];
        for (registration, fix) in cases {
            let message = registration_message(&registration)
                .unwrap_or_else(|| panic!("{registration:?} must be reported"));
            assert!(message.contains(fix), "{registration:?}: {message}");
        }
        assert_eq!(registration_message(&DistroRegistration::Usable), None);
    }

    #[test]
    fn check_output_is_read_past_other_output() {
        assert_eq!(
            parse_check_output(
                "motd noise\nzed-wsl-helper: ready /home/my user/.cache/zed-jp-enhance/sandbox/k/zed-remote-server\n"
            )
            .unwrap(),
            HelperCheck::Ready(
                "/home/my user/.cache/zed-jp-enhance/sandbox/k/zed-remote-server".to_string()
            )
        );
        assert_eq!(
            parse_check_output("zed-wsl-helper: missing /root/.cache/zed-jp-enhance/sandbox/k\n")
                .unwrap(),
            HelperCheck::Missing
        );
        assert_eq!(
            parse_check_output(
                "zed-wsl-helper: unverifiable /root/.cache/zed-jp-enhance/sandbox/k\n"
            )
            .unwrap(),
            HelperCheck::Unverifiable
        );
        assert!(parse_check_output("").is_err());
        assert!(parse_check_output("zed-wsl-helper: ready relative\n").is_err());
        assert!(parse_check_output("zed-wsl-helper: maybe /x\n").is_err());
    }

    #[test]
    fn stage_output_keeps_spaces_in_the_staged_path() {
        assert_eq!(
            parse_stage_output(
                "zed-wsl-install: 1234 abcdef /home/my user/.cache/zed-jp-enhance/sandbox/k/.slirp4netns.AbC123\n"
            )
            .unwrap(),
            StageReport {
                size: 1234,
                sha256: Some("abcdef".to_string()),
                path: "/home/my user/.cache/zed-jp-enhance/sandbox/k/.slirp4netns.AbC123".to_string(),
            }
        );
        // Without `sha256sum` only the size can be compared.
        assert_eq!(
            parse_stage_output("zed-wsl-install: 7 - /tmp/x\n")
                .unwrap()
                .sha256,
            None
        );
        assert!(parse_stage_output("zed-wsl-install: many - /tmp/x\n").is_err());
        assert!(parse_stage_output("zed-wsl-install: 7 -\n").is_err());
    }

    #[test]
    fn helper_probe_output_is_parsed_and_explained() {
        let healthy = parse_helper_probe("zed-wsl-probe: landlock=7 userns=ok\n").unwrap();
        assert_eq!(probe_problem(&healthy, None), None);

        let no_landlock = parse_helper_probe("zed-wsl-probe: landlock=0 userns=ok\n").unwrap();
        let message = probe_problem(&no_landlock, Some("Ubuntu")).expect("must be reported");
        assert!(message.contains("Landlock") && message.contains("wsl --update"));

        let no_namespaces =
            parse_helper_probe("zed-wsl-probe: landlock=7 userns=errno-1\n").unwrap();
        let message = probe_problem(&no_namespaces, None).expect("must be reported");
        assert!(message.contains("errno-1") && message.contains("wsl --update"));

        assert!(parse_helper_probe("").is_err());
        assert!(parse_helper_probe("zed-wsl-probe: userns=ok\n").is_err());
    }

    #[test]
    fn install_key_depends_on_both_artifacts() {
        let key = helper_install_key("server-a", "slirp-a");
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(key, helper_install_key("server-a", "slirp-a"));
        assert_ne!(key, helper_install_key("server-b", "slirp-a"));
        assert_ne!(key, helper_install_key("server-a", "slirp-b"));
    }

    #[test]
    fn artifact_names_prefer_the_compressed_server() {
        let ([compressed, plain], slirp) = helper_artifact_names();
        let architecture = std::env::consts::ARCH;
        assert_eq!(
            compressed,
            format!("zed-remote-server-linux-{architecture}.gz")
        );
        assert_eq!(plain, format!("zed-remote-server-linux-{architecture}"));
        assert_eq!(slirp, format!("slirp4netns-linux-{architecture}"));
    }

    #[test]
    fn scripts_use_the_installed_file_names() {
        for script in [CHECK_SCRIPT, FINALIZE_SCRIPT] {
            assert!(script.contains("installed"));
        }
        assert!(CHECK_SCRIPT.contains(&format!("$dir/{HELPER_BINARY_NAME}")));
        assert!(CHECK_SCRIPT.contains(&format!("$dir/{SLIRP_BINARY_NAME}")));
    }

    #[test]
    fn wsl_output_is_decoded_from_utf16_or_utf8() {
        let utf16: Vec<u8> = "エラー: distro"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(decode_wsl_text(&utf16), "エラー: distro");
        assert_eq!(decode_wsl_text("plain output".as_bytes()), "plain output");
        assert_eq!(decode_wsl_text(b""), "");
    }

    #[test]
    fn parse_wsl_localhost_path() {
        let path = parse_wsl_unc_path(r"\\wsl.localhost\Ubuntu\home\me\project").unwrap();
        assert_eq!(path.distro.as_deref(), Some("Ubuntu"));
        assert_eq!(path.path, "/home/me/project");
    }

    #[test]
    fn parse_wsl_dollar_path() {
        let path = parse_wsl_unc_path(r"\\wsl$\Debian\tmp").unwrap();
        assert_eq!(path.distro.as_deref(), Some("Debian"));
        assert_eq!(path.path, "/tmp");
    }

    #[test]
    fn parse_native_windows_path() {
        let path = parse_native_drive_path(r"C:\Users\me\project").unwrap();
        assert_eq!(path.distro, None);
        assert_eq!(path.path, "/mnt/c/Users/me/project");
    }

    #[test]
    fn parse_wsl_absolute_path_keeps_linux_path() {
        let path = parse_wsl_absolute_path("/home/me").unwrap();
        assert_eq!(path.distro, None);
        assert_eq!(path.path, "/home/me");
    }

    #[test]
    fn parse_wsl_absolute_path_rejects_unc_paths() {
        assert!(parse_wsl_absolute_path(r"\\server\share").is_err());
    }

    #[test]
    fn parse_verbatim_native_windows_path() {
        let path = parse_native_drive_path(r"\\?\D:\workspace").unwrap();
        assert_eq!(path.distro, None);
        assert_eq!(path.path, "/mnt/d/workspace");
    }

    #[test]
    fn rejects_unc_non_wsl_path() {
        assert!(parse_native_drive_path(r"\\server\share\project").is_err());
    }

    #[test]
    fn canonical_grant_output_returns_the_linux_canonical_path() {
        let resolved =
            parse_canonical_grant_output("Ubuntu-24.04\n/home/me/real target\nnative-fs\n")
                .expect("valid helper output");
        // The bare Linux path, not a `\\wsl.localhost\...` spelling.
        assert_eq!(resolved.canonical, PathBuf::from("/home/me/real target"));
        assert!(!resolved.on_windows_fs);

        let resolved = parse_canonical_grant_output("Ubuntu\n/mnt/c/Users/me/proj\nwindows-fs\n")
            .expect("valid helper output");
        assert_eq!(resolved.canonical, PathBuf::from("/mnt/c/Users/me/proj"));
        assert!(resolved.on_windows_fs);
    }

    #[test]
    fn canonical_grant_output_rejects_malformed_protocol() {
        // Missing filesystem-class line.
        assert!(parse_canonical_grant_output("Ubuntu\n/home/me/project\n").is_err());
        // Non-absolute canonical path.
        assert!(parse_canonical_grant_output("Ubuntu\nrelative\nnative-fs\n").is_err());
        // Distro name containing a separator.
        assert!(parse_canonical_grant_output("Ubuntu/bad\n/home/me/project\nnative-fs\n").is_err());
        // Unknown filesystem class.
        assert!(parse_canonical_grant_output("Ubuntu\n/home/me/project\nmaybe-fs\n").is_err());
        // Trailing junk after the filesystem class.
        assert!(
            parse_canonical_grant_output("Ubuntu\n/home/me/project\nnative-fs\nnoise\n").is_err()
        );
    }

    #[test]
    fn split_resolved_paths_keeps_existing_protected_paths_and_skips_missing_ones() {
        let (cwd, writable_paths, protected_paths) = split_resolved_paths(
            true,
            1,
            vec![
                Some("/home/me/project".to_string()),
                Some("/home/me/project".to_string()),
                None,
                Some("/mnt/c/external/.git".to_string()),
            ],
        )
        .unwrap();

        assert_eq!(cwd.as_deref(), Some("/home/me/project"));
        assert_eq!(writable_paths, vec!["/home/me/project".to_string()]);
        assert_eq!(protected_paths, vec!["/mnt/c/external/.git".to_string()]);
    }

    #[test]
    fn split_resolved_paths_rejects_missing_required_writable_paths() {
        let error = split_resolved_paths(false, 1, vec![None]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("required writable path resolved as missing"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn select_distro_uses_wsl_distro_when_present() {
        let distro = select_distro(
            None,
            &[
                PathMapping::NativeDrive {
                    windows_path: "C:/project".to_string(),
                    fallback: WslPath {
                        distro: None,
                        path: "/mnt/c/project".to_string(),
                    },
                },
                PathMapping::Wsl(WslPath {
                    distro: Some("Ubuntu".to_string()),
                    path: "/home/me/project".to_string(),
                }),
            ],
        )
        .unwrap();
        assert_eq!(distro.as_deref(), Some("Ubuntu"));
    }

    #[test]
    fn bad_request_errors_do_not_claim_sandboxing_is_unavailable() {
        // Mixed distros and missing/unmappable paths are model-fixable bad
        // requests. They must not be typed as `WslSandboxUnavailable` (nor
        // carry its prefix), since the agent uses that type to ask the user to
        // fix their environment.
        let mixed_distros = select_distro(
            Some(&PathMapping::Wsl(WslPath {
                distro: Some("Ubuntu".to_string()),
                path: "/home/me".to_string(),
            })),
            &[PathMapping::Wsl(WslPath {
                distro: Some("Debian".to_string()),
                path: "/home/me".to_string(),
            })],
        )
        .unwrap_err();
        assert!(
            mixed_distros
                .downcast_ref::<WslSandboxUnavailable>()
                .is_none()
        );
        assert!(!format!("{mixed_distros:#}").contains(WSL_SANDBOX_UNAVAILABLE_PREFIX));

        let missing_path =
            path_to_wsl(Path::new(r"C:\zed-test\definitely\does\not\exist-2769")).unwrap_err();
        assert!(
            missing_path
                .downcast_ref::<WslSandboxUnavailable>()
                .is_none()
        );
        assert!(!format!("{missing_path:#}").contains(WSL_SANDBOX_UNAVAILABLE_PREFIX));

        let unmappable_cwd = directory_to_wsl(Path::new(r"\\server\share\project")).unwrap_err();
        assert!(
            unmappable_cwd
                .downcast_ref::<WslSandboxUnavailable>()
                .is_none()
        );
        assert!(!format!("{unmappable_cwd:#}").contains(WSL_SANDBOX_UNAVAILABLE_PREFIX));
    }

    #[test]
    fn unavailable_errors_are_typed_and_prefixed() {
        // Environment failures are recognizable by type (so the agent doesn't
        // depend on message text) and still render with the shared prefix.
        let message = registration_message(&DistroRegistration::NotInstalled).unwrap();
        let error = unavailable(message.clone());
        let typed = error
            .downcast_ref::<WslSandboxUnavailable>()
            .expect("environment failure should downcast to WslSandboxUnavailable");
        assert_eq!(typed.message(), message);
        assert!(format!("{error:#}").starts_with(WSL_SANDBOX_UNAVAILABLE_PREFIX));
    }

    #[test]
    fn map_path_to_wsl_keeps_unc_paths_structural() {
        let mapping = map_path_to_wsl(Path::new(r"\\wsl.localhost\Ubuntu\home\me")).unwrap();
        assert_eq!(
            mapping,
            PathMapping::Wsl(WslPath {
                distro: Some("Ubuntu".to_string()),
                path: "/home/me".to_string(),
            })
        );
    }

    #[test]
    fn map_path_to_wsl_defers_native_paths_to_wslpath() {
        let mapping = map_path_to_wsl(Path::new(r"C:\Users\me\project")).unwrap();
        assert_eq!(
            mapping,
            PathMapping::NativeDrive {
                windows_path: "C:/Users/me/project".to_string(),
                fallback: WslPath {
                    distro: None,
                    path: "/mnt/c/Users/me/project".to_string(),
                },
            }
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_only_strips_drive_paths() {
        // `canonicalize` yields `\\?\C:\...`; the plain native spelling is stored.
        assert_eq!(
            strip_windows_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\me\dir")),
            PathBuf::from(r"C:\Users\me\dir")
        );
        // A `\\?\UNC\...` path is not a drive path and is left intact (the caller
        // rejects it).
        assert_eq!(
            strip_windows_verbatim_prefix(PathBuf::from(r"\\?\UNC\wsl.localhost\Ubuntu\home")),
            PathBuf::from(r"\\?\UNC\wsl.localhost\Ubuntu\home")
        );
        // A path without the prefix is unchanged.
        assert_eq!(
            strip_windows_verbatim_prefix(PathBuf::from(r"C:\already\plain")),
            PathBuf::from(r"C:\already\plain")
        );
    }

    #[test]
    fn map_path_to_wsl_strips_verbatim_prefix_for_wslpath() {
        let mapping = map_path_to_wsl(Path::new(r"\\?\D:\workspace")).unwrap();
        assert_eq!(
            mapping,
            PathMapping::NativeDrive {
                windows_path: "D:/workspace".to_string(),
                fallback: WslPath {
                    distro: None,
                    path: "/mnt/d/workspace".to_string(),
                },
            }
        );
    }

    #[test]
    fn path_resolution_args_flattens_mappings_into_triples() {
        let mappings = [
            PathMapping::NativeDrive {
                windows_path: "C:/Users/me/project".to_string(),
                fallback: WslPath {
                    distro: None,
                    path: "/mnt/c/Users/me/project".to_string(),
                },
            },
            PathMapping::Wsl(WslPath {
                distro: Some("Ubuntu".to_string()),
                path: "/home/me/project".to_string(),
            }),
        ];
        assert_eq!(
            path_resolution_args(mappings.iter()),
            [
                "W",
                "C:/Users/me/project",
                "/mnt/c/Users/me/project",
                "L",
                "/home/me/project",
                "",
            ]
        );
    }

    #[test]
    fn parse_path_resolution_output_reads_one_line_per_path() {
        let resolved = parse_path_resolution_output(
            "ok ok /mnt/c/Users/me/project\nfallback missing /mnt/d/workspace\n",
            2,
        )
        .unwrap();
        assert_eq!(
            resolved,
            [
                ResolvedPath {
                    path: "/mnt/c/Users/me/project".to_string(),
                    used_fallback: false,
                    exists: true,
                },
                ResolvedPath {
                    path: "/mnt/d/workspace".to_string(),
                    used_fallback: true,
                    exists: false,
                },
            ]
        );
    }

    #[test]
    fn parse_path_resolution_output_keeps_spaces_in_paths() {
        let resolved =
            parse_path_resolution_output("ok ok /mnt/c/Users/me/My Documents/project\n", 1)
                .unwrap();
        assert_eq!(resolved[0].path, "/mnt/c/Users/me/My Documents/project");
    }

    #[test]
    fn parse_path_resolution_output_rejects_wrong_line_count() {
        assert!(parse_path_resolution_output("ok ok /a\n", 2).is_err());
        assert!(parse_path_resolution_output("ok ok /a\nok ok /b\n", 1).is_err());
    }

    #[test]
    fn parse_path_resolution_output_rejects_corrupted_lines() {
        assert!(parse_path_resolution_output("garbage\n", 1).is_err());
        assert!(parse_path_resolution_output("weird ok /a\n", 1).is_err());
        assert!(parse_path_resolution_output("ok weird /a\n", 1).is_err());
        assert!(parse_path_resolution_output("ok ok not-absolute\n", 1).is_err());
    }
}
