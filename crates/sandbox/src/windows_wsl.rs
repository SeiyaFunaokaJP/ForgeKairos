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

use crate::protected_paths::{
    AGENT_BINARIES_HOME_RELATIVE, IDE_RENDEZVOUS_HOME_RELATIVE, WSL_SANDBOX_HELPER_HOME_RELATIVE,
};
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
    /// ADR-F49: a long-running interactive program rather than a one-shot
    /// command.
    pub(crate) interactive: bool,
    /// ADR-F49: the IDE channel this command reaches Zed over, if it has one.
    /// Merged by [`crate::Sandbox::wrap`] from the interactive session's
    /// (decision 3) and the one a one-shot ACP agent is given on its own
    /// (decision 24).
    pub(crate) ide_forward: Option<crate::IdeForward>,
    /// ADR-F49 第 2b 段: the command and its environment were written for
    /// Linux rather than picked up from this Windows process, so the
    /// environment is passed on as it is (see [`linux_environment`]).
    pub(crate) linux_environment: bool,
    /// ADR-F49 決定 18: the name this sandbox is registered under, so what the
    /// command leaves running can be followed from outside it.
    pub(crate) sandbox_session_id: Option<String>,
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

/// Copies one file into a directory below `$HOME` — or, when `$1` is itself an
/// absolute Linux path, into that directory (ADR-F49 第 2b 段: a runtime
/// archive goes into the distro's Zed data directory, which `XDG_DATA_HOME` can
/// move out of the account's home): stdin goes to a fresh temporary file there,
/// whose size, SHA-256 (`-` without `sha256sum`) and path are reported for
/// [`stage_file`] to check against what it sent. `$2` is the file's final name.
const STAGE_SCRIPT: &str = r#"
set -eu
case "$1" in
    /*) dir="$1" ;;
    *) dir="$HOME/$1" ;;
esac
mkdir -p "$dir"
staged=$(mktemp "$dir/.$2.XXXXXX")
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
/// docs). The message is the explanation of what to fix, built both for the
/// user, who is shown it, and in English for the agent, which is told it. Returned as the root of the `anyhow::Error` so callers
/// classify it by type ([`anyhow::Error::downcast_ref`]) instead of by matching
/// message text. Per-request failures (a missing writable path, paths mixing
/// distros) are ordinary `anyhow` errors *without* this type.
#[derive(Debug, Clone)]
pub struct WslSandboxUnavailable(i18n::Bilingual);

impl WslSandboxUnavailable {
    /// Build an environment-unavailable error from a user-facing reason
    /// (without the [`WSL_SANDBOX_UNAVAILABLE_PREFIX`], which `Display` adds).
    /// `message` is called once per language, so it must only build the text.
    pub fn new<S: Into<String>>(message: impl Fn() -> S) -> Self {
        Self(i18n::Bilingual::new(|| message().into()))
    }

    /// The reason, without the leading [`WSL_SANDBOX_UNAVAILABLE_PREFIX`].
    #[cfg(test)]
    pub fn message(&self) -> &i18n::Bilingual {
        &self.0
    }
}

impl std::fmt::Display for WslSandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {}",
            i18n::tr(WSL_SANDBOX_UNAVAILABLE_PREFIX),
            self.0
        )
    }
}

impl std::error::Error for WslSandboxUnavailable {}

/// Shorthand for an [`anyhow::Error`] wrapping a [`WslSandboxUnavailable`].
fn unavailable<S: Into<String>>(message: impl Fn() -> S) -> anyhow::Error {
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

/// ADR-F49 第 2b 段: the WSL distribution a sandboxed **session** runs in — the
/// one its project's folders name (`project::agent_sandbox::wsl_distro_for_roots`,
/// which [`select_distro`] then re-derives from the very same paths), or the
/// default distro, whose registered name Zed deliberately never learns.
///
/// This is not the distribution a *path* names, and the two must not be
/// confused: a grant spelled `\\wsl.localhost\<distro>\…` pins its own, while a
/// bare Linux-absolute grant (`/home/me/x`) names none and only means anything
/// relative to the session's (see [`resolve_canonical_for_grant`]). Hence a type
/// of its own rather than one more `Option<&str>` alongside them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WslSessionDistro(Option<String>);

impl WslSessionDistro {
    /// The session runs in whichever distribution WSL calls the default one.
    pub fn default_distro() -> Self {
        Self(None)
    }

    /// The session runs in the named distribution. WSL matches the name
    /// case-insensitively, so the case it is given in does not matter.
    pub fn named(distro: impl Into<String>) -> Self {
        Self(Some(distro.into()))
    }

    /// The distribution's registered name, or `None` for the default distro.
    pub fn name(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl From<Option<String>> for WslSessionDistro {
    /// The shape `project::agent_sandbox::wsl_distro_for_roots` reports a
    /// project's distribution in: `None` is the default distro.
    fn from(distro: Option<String>) -> Self {
        Self(distro)
    }
}

/// The distribution a write grant is resolved, shown and stored in: the one the
/// request named through a share, else the session's (see
/// [`WslSessionDistro`]). `None` means the default distro, which is the only
/// case where a bare `/…` grant stays a bare Linux path.
///
/// A grant naming another distribution than the session's is refused here rather
/// than approved: [`select_distro`] cannot serve a command whose paths span two
/// distributions, so the user would be asked to approve a folder no command
/// could ever write.
fn grant_distro(requested: Option<&str>, session: &WslSessionDistro) -> Result<Option<String>> {
    match (requested, session.name()) {
        (Some(requested), Some(session)) => {
            ensure!(
                requested.eq_ignore_ascii_case(session),
                i18n::t!(
                    "The path is in the WSL distro `{requested}`, but this session runs in \
                     `{session}`."
                )
            );
            Ok(Some(requested.to_string()))
        }
        (Some(requested), None) => Ok(Some(requested.to_string())),
        (None, session) => Ok(session.map(str::to_string)),
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
    let distros = registered_distros(&lxss);
    if distros.is_empty() && !wsl_installed() {
        return DistroRegistration::NotInstalled;
    }
    let default_id = lxss.get_string("DefaultDistribution").ok();
    classify_registration(&distros, default_id.as_deref(), distro)
}

fn registered_distros(lxss: &windows_registry::Key) -> Vec<RegisteredDistro> {
    match lxss.keys() {
        Ok(keys) => keys
            .filter_map(|id| {
                let key = lxss.open(&id).ok()?;
                let name = key.get_string("DistributionName").ok()?;
                let version = key.get_u32("Version").ok();
                Some(RegisteredDistro { id, name, version })
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// ADR-F49: the name WSL registers `distro` under, or the default distro's
/// for `None` — one spelling for each distro, whether a `\\wsl.localhost`
/// path or the default named it. `None` when the registry does not say.
pub fn registered_wsl_distro_name(distro: Option<&str>) -> Option<String> {
    let lxss = windows_registry::CURRENT_USER.open(LXSS_KEY).ok()?;
    let default_id = lxss.get_string("DefaultDistribution").ok();
    registered_name(&registered_distros(&lxss), default_id.as_deref(), distro)
}

fn registered_name(
    distros: &[RegisteredDistro],
    default_id: Option<&str>,
    requested: Option<&str>,
) -> Option<String> {
    let distro = match requested {
        Some(name) => distros
            .iter()
            .find(|distro| distro.name.eq_ignore_ascii_case(name)),
        None => default_id.and_then(|id| {
            distros
                .iter()
                .find(|distro| distro.id.eq_ignore_ascii_case(id))
        }),
    }?;
    Some(distro.name.clone())
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
            i18n::t!(
                "WSL is not installed. On Windows, agent commands run in a sandbox inside WSL 2. \
                 Open PowerShell as an administrator, run `wsl --install`, and restart the computer."
            )
            .to_string(),
        ),
        DistroRegistration::NoDistro => Some(
            i18n::t!(
                "No WSL distro is registered. Add one with `wsl --install -d Ubuntu` or similar \
                 (any distro works, but it must run on WSL 2)."
            )
            .to_string(),
        ),
        DistroRegistration::NoDefault => Some(
            i18n::t!(
                "No default WSL distro is set. List the distros with `wsl --list --verbose` and make \
                 a WSL 2 distro the default with `wsl --set-default <name>`."
            )
            .to_string(),
        ),
        DistroRegistration::Missing(name) => Some(i18n::t!(
            "The WSL distro `{name}` is not registered. Check its name with `wsl --list --verbose`."
        )),
        DistroRegistration::Wsl1(name) => Some(i18n::t!(
            "The WSL distro `{name}` runs on WSL 1, but the sandbox needs WSL 2. Convert it with \
             `wsl --set-version {name} 2`, or make a WSL 2 distro the default with \
             `wsl --set-default <name>`."
        )),
    }
}

fn ensure_distro_registered(distro: Option<&str>) -> Result<()> {
    let registration = read_distro_registration(distro);
    match registration_message(&registration) {
        Some(_) => Err(unavailable(|| {
            registration_message(&registration).unwrap_or_default()
        })),
        None => Ok(()),
    }
}

/// Resolve a requested write grant to the path we persist for it, in
/// **platform-native** form: a Windows drive path stays `C:\...` (NTFS), a
/// Linux-absolute path stays `/...`, and a `\\wsl.localhost\<distro>\...` share
/// stays a share of the same distro. In every case the path is resolved to its
/// symlink/junction-free canonical target (so a granted junction or symlink is
/// followed and shown/stored as its real target), and its backing filesystem is
/// classified.
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
/// A grant named through a share pins a distribution and keeps it — the same
/// `/root/p` is a different folder in a different distribution, so dropping the
/// distribution would widen the grant to whichever one the next command runs in.
/// A bare Linux path (`/home/me/x`) names no distribution of its own, so it is
/// resolved in `session`'s: the sandboxed command runs there, and resolving
/// anywhere else would show the user a folder in one distribution while the
/// command wrote in another. Its canonical is then stored as that
/// distribution's share too, so the approval dialog and the persisted grant say
/// which distribution it is in. Only a session in the **default** distro
/// ([`WslSessionDistro::default_distro`]) leaves a bare path bare: there is
/// nothing else it could mean, and Zed deliberately never learns that distro's
/// registered name (ADR-F49 decision 33).
pub async fn resolve_canonical_for_grant(
    requested: PathBuf,
    session: WslSessionDistro,
) -> Result<ResolvedGrant> {
    let path_string = requested.to_string_lossy();

    // A Windows drive grant (NTFS) is resolved on the Windows host, which is
    // authoritative about junctions and symlinks (see the doc comment above).
    if parse_native_drive_path(&path_string).is_ok() {
        let canonical = smol::fs::canonicalize(&requested).await.with_context(|| {
            i18n::t!(
                "Could not resolve the writable path `{path}` on Windows.",
                path = requested.display()
            )
        })?;
        // `canonicalize` yields a `\\?\` verbatim path; store the plain native
        // spelling so a non-junction grant compares equal to what was requested.
        // A junction resolving off the local drives (onto a UNC/WSL path) is
        // rejected — only a Windows drive path maps to a DrvFs path.
        let canonical = strip_windows_verbatim_prefix(canonical);
        ensure!(
            parse_native_drive_path(&canonical.to_string_lossy()).is_ok(),
            i18n::t!(
                "Writing to `{path}` cannot be allowed: it resolves to `{resolved}`, \
                 which is not a path on a Windows drive.",
                path = requested.display(),
                resolved = canonical.display()
            )
        );
        return Ok(ResolvedGrant {
            canonical,
            on_windows_fs: true,
        });
    }

    // Otherwise the grant names a location on a distro's own filesystem, which
    // only WSL can canonicalize: a Linux-absolute path in the distro the command
    // runs in, or a `\\wsl.localhost\<distro>\…` share naming one distro.
    let wsl_path = parse_distro_native_path(&path_string).map_err(|_| {
        anyhow::anyhow!(i18n::t!(
            "Writing to `{path}` cannot be allowed: only a path on a Windows drive (`C:\\...`), \
             an absolute WSL path (`/...`), or a folder inside a WSL distro \
             (`\\\\wsl.localhost\\<distro>\\...`) can be given.",
            path = requested.display()
        ))
    })?;
    // A share with nothing under it, or a bare `/`, is the distribution's whole
    // root, which is never the folder the user was shown. A bare one is refused
    // here too: it is the same place, and once a session names a distribution
    // (below) it would otherwise be stored as that distribution's bare share —
    // a shape `require_windows_grant_shape` refuses, so the grant would be
    // approved and then never usable.
    ensure!(
        wsl_path.path != "/",
        i18n::t!(
            "Writing to `{path}` cannot be allowed: it is a whole WSL distro, not a folder inside one.",
            path = requested.display()
        )
    );
    let distro = grant_distro(wsl_path.distro.as_deref(), &session).with_context(|| {
        i18n::t!(
            "Writing to `{path}` cannot be allowed",
            path = requested.display()
        )
    })?;
    let mapping = PathMapping::Wsl(wsl_path);

    ensure_distro_registered(distro.as_deref())?;
    let wsl_exe = wsl_exe_path();
    if !wsl_exe.is_file() {
        return Err(unavailable(|| {
            registration_message(&DistroRegistration::NotInstalled).unwrap_or_default()
        }));
    }

    let translated = resolve_paths(
        &wsl_exe,
        distro.as_deref(),
        &[(mapping, "writable path", true)],
    )
    .await?
    .into_iter()
    .next()
    .context(i18n::t!(
        "The writable path was not resolved (internal error)"
    ))?
    .context(i18n::t!(
        "The writable path was passed on unresolved (internal error)"
    ))?;
    let helper = ensure_sandbox_helper(&wsl_exe, distro.as_deref()).await?;
    let mut args = helper_invocation(&helper);
    args.extend([crate::WSL_SANDBOX_RESOLVE_FLAG.to_string(), translated]);
    let output = run_wsl_command(
        &wsl_exe,
        distro.as_deref(),
        &args,
        i18n::tc!("wsl sandbox step", "resolving the writable path"),
    )
    .await?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(i18n::t!(
            "Could not resolve the writable path `{path}`.{details}",
            path = requested.display(),
            details = command_failure_details(output.status.code(), &output.stderr)
        )));
    }
    let stdout = String::from_utf8(output.stdout).context(i18n::t!(
        "The WSL sandbox helper returned a canonical path that is not UTF-8 (internal error)"
    ))?;
    parse_canonical_grant_output(&stdout, distro.as_deref())
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
#[allow(
    clippy::too_many_arguments,
    reason = "each part of the policy is mapped into WSL on its own"
)]
pub async fn wrap_invocation<S: BuildHasher>(
    program: String,
    args: Vec<String>,
    writable_paths: Vec<WritablePath>,
    protected_paths: Vec<PathBuf>,
    readable_paths: Vec<PathBuf>,
    permissions: SandboxPermissions,
    cwd: Option<PathBuf>,
    env: HashMap<String, String, S>,
) -> Result<(String, Vec<String>)> {
    if let Some(forward) = permissions.ide_forward.as_ref() {
        ensure!(
            forward.inner_port != 0,
            i18n::t!("The IDE port inside the sandbox is 0 (internal error)")
        );
        ensure_linux_absolute_path(
            &forward.rendezvous_path,
            i18n::tc!("wsl sandbox path", "IDE rendezvous socket"),
        )?;
    }

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
    // What the command may read among the hidden entries has to be there: the
    // helper refuses the whole command over one it cannot check.
    let readable_mappings = readable_paths
        .iter()
        .map(|path| {
            path_to_wsl(path).with_context(|| {
                format!("failed to map readable path `{}` into WSL", path.display())
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let distro = select_distro(
        cwd_mapping.as_ref(),
        writable_requested_mappings
            .iter()
            .chain(writable_mappings.iter())
            .chain(protected_mappings.iter())
            .chain(readable_mappings.iter()),
    )?;
    ensure_distro_registered(distro.as_deref())?;
    let wsl_exe = wsl_exe_path();
    if !wsl_exe.is_file() {
        return Err(unavailable(|| {
            registration_message(&DistroRegistration::NotInstalled).unwrap_or_default()
        }));
    }
    let helper = ensure_sandbox_helper(&wsl_exe, distro.as_deref()).await?;

    // Resolve all paths (translating native drive-letter paths with `wslpath`
    // now that the distro is known) in a single WSL round-trip. The cwd and
    // writable paths must exist; protected paths may be missing — the helper
    // skips those, and would itself protect the distro account's home.
    let has_cwd = cwd_mapping.is_some();
    let writable_path_count = writable_mappings.len();
    let readable_path_count = readable_mappings.len();
    let mut mappings = Vec::with_capacity(
        writable_mappings.len() + readable_mappings.len() + protected_mappings.len() + 1,
    );
    if let Some(mapping) = cwd_mapping {
        mappings.push((mapping, "terminal cwd", true));
    }
    mappings.extend(
        writable_mappings
            .into_iter()
            .map(|mapping| (mapping, "writable path", true)),
    );
    mappings.extend(
        readable_mappings
            .into_iter()
            .map(|mapping| (mapping, "readable path", true)),
    );
    mappings.extend(
        protected_mappings
            .into_iter()
            .map(|mapping| (mapping, "protected path", false)),
    );
    let resolved = resolve_paths(&wsl_exe, distro.as_deref(), &mappings).await?;
    let (cwd, writable_paths, readable_paths, protected_paths) =
        split_resolved_paths(has_cwd, writable_path_count, readable_path_count, resolved)?;

    let environment = if permissions.linux_environment {
        linux_environment(&env)
    } else {
        forwarded_environment(&env)
    };
    let helper_argv = crate::build_wsl_sandbox_helper_argv(
        &writable_paths,
        &protected_paths,
        &readable_paths,
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
            interactive: permissions.interactive,
            ide_forward: permissions.ide_forward.as_ref(),
            sandbox_session_id: permissions.sandbox_session_id.as_deref(),
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

/// Read what `--wsl-sandbox-resolve-path` printed. `requested_distro` is the
/// distribution the grant is resolved in — the one the request named through
/// its share, else the session's ([`grant_distro`]) — and the canonical is then
/// spelled back as a share of that distribution instead of as a bare Linux
/// path. `None` is the default distro, the only case where nothing names a
/// distribution and the canonical stays a bare Linux path (see
/// [`resolve_canonical_for_grant`]).
fn parse_canonical_grant_output(
    stdout: &str,
    requested_distro: Option<&str>,
) -> Result<ResolvedGrant> {
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
            && !distro.contains(['/', '\\', '\0'])
            && distro != "."
            && distro != ".."
            && canonical.starts_with('/'),
        "WSL sandbox helper returned invalid canonical-path output: {stdout:?}"
    );
    let Some(requested_distro) = requested_distro else {
        // The default distro: nothing named a distribution, and Zed does not
        // learn this one's name, so the canonical stays the Linux path the
        // helper granted. Spelling it as `\\wsl.localhost\<distro>\...` would
        // resurface the Windows-style path we avoid and, because it names the
        // same object as the request, show as a spurious redirect in the
        // approval UI.
        return Ok(ResolvedGrant {
            canonical: PathBuf::from(canonical),
            on_windows_fs,
        });
    };
    // The helper reports the distro it actually ran in (`WSL_DISTRO_NAME`).
    // `wsl.exe -d` matches the requested name case-insensitively, so anything
    // else means the grant was resolved somewhere other than where it was asked
    // for — fail rather than persist a path pointing at the wrong distribution.
    ensure!(
        distro.eq_ignore_ascii_case(requested_distro),
        "the WSL sandbox helper resolved a grant for distribution `{requested_distro}` \
         in `{distro}` instead"
    );
    // A canonical the share spelling cannot carry back unchanged is refused
    // rather than stored:
    //
    // * `\` is an ordinary character in a Linux file name and the separator
    //   in a share path, so `/root/a\b` would be stored as a path that reads
    //   back as `/root/a/b` — a different object, and one nobody approved;
    // * `/` is the distribution's root, which spells a bare share, a shape
    //   `require_windows_grant_shape` refuses — the grant would be approved
    //   here and then never usable.
    ensure!(
        !canonical.contains('\\'),
        "the WSL sandbox helper resolved a grant to {canonical:?}, which has no \
         Windows spelling"
    );
    ensure!(
        canonical != "/",
        "the WSL sandbox helper resolved a grant to the whole root of \
         distribution `{distro}` rather than to a folder in it"
    );
    // ADR-F49 第 2b 段: spell the canonical as a share of the distribution it
    // was resolved in, so the stored grant still says which distribution that
    // is and compares, component by component, with the worktree roots of an
    // ext4 project — which reach the sandbox spelled the same way. For a bare
    // `/…` grant in a non-default distro this is also what the approval dialog
    // shows, which is the point: the folder the user sees is the folder the
    // command will be able to write.
    Ok(ResolvedGrant {
        canonical: PathBuf::from(crate::util::wsl_share_path_spelling(distro, canonical)),
        on_windows_fs,
    })
}

fn split_resolved_paths(
    has_cwd: bool,
    writable_path_count: usize,
    readable_path_count: usize,
    resolved: Vec<Option<String>>,
) -> Result<(Option<String>, Vec<String>, Vec<String>, Vec<String>)> {
    let mut resolved = resolved.into_iter();
    let cwd = if has_cwd {
        Some(
            resolved
                .next()
                .context(i18n::t!(
                    "The terminal's working folder was not resolved (internal error)"
                ))?
                .context(i18n::t!(
                    "The terminal's working folder was passed on unresolved (internal error)"
                ))?,
        )
    } else {
        None
    };

    let mut writable_paths = Vec::with_capacity(writable_path_count);
    for _ in 0..writable_path_count {
        writable_paths.push(
            resolved
                .next()
                .context(i18n::t!(
                    "The writable path was not resolved (internal error)"
                ))?
                .context(i18n::t!(
                    "The writable path was passed on unresolved (internal error)"
                ))?,
        );
    }

    let mut readable_paths = Vec::with_capacity(readable_path_count);
    for _ in 0..readable_path_count {
        readable_paths.push(
            resolved
                .next()
                .context(i18n::t!(
                    "The readable path was not resolved (internal error)"
                ))?
                .context(i18n::t!(
                    "The readable path was passed on unresolved (internal error)"
                ))?,
        );
    }

    Ok((
        cwd,
        writable_paths,
        readable_paths,
        resolved.flatten().collect(),
    ))
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
            // single sandboxed command can't serve. Two *spellings* of one
            // distro are not that: WSL matches a distro name
            // case-insensitively, and so does
            // `project::agent_sandbox::wsl_distro_for_roots`, which decides the
            // session's distro from these very paths.
            Some(distro) => ensure!(
                distro.eq_ignore_ascii_case(path_distro),
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
/// `~/.cache/forgekairos/sandbox/<key>/` inside the distro — nothing is
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
            let server_digest = file_sha256(&artifacts.server, StagedKind::SandboxHelper).await?;
            let slirp_digest = file_sha256(&artifacts.slirp, StagedKind::SandboxHelper).await?;
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
    if probe_problem(&probe, distro).is_some() {
        return Err(unavailable(|| {
            probe_problem(&probe, distro).unwrap_or_default()
        }));
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
        _ => Err(unavailable(|| {
            i18n::t!(
                "Set the environment variable `ZED_WSL_SANDBOX_HELPER` to an absolute path inside WSL (UTF-8)."
            )
        })),
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
                .join(i18n::tc!("list separator", ", "));
            Err(unavailable(|| {
                i18n::t!(
                    "The sandbox helpers (`{server_name}` and `{slirp_name}`) were not found where Zed \
                 is installed. Reinstall Zed. In a development build, `script/build-dev.ps1` puts them \
                 in `target\\debug` (searched: {searched}).",
                    server_name = server_names[0]
                )
            }))
        }
    }
}

/// What [`stage_file`] copies into the distro, for its messages.
#[derive(Clone, Copy, Debug)]
enum StagedKind {
    /// The helper and `slirp4netns`, shipped with Zed.
    SandboxHelper,
    /// An agent binary Zed obtained for the sandbox (ADR-F49).
    AgentBinary,
    /// ADR-F49 第 2b 段: an archive Zed obtained for the Linux runtime an
    /// agent runs in inside WSL — the Linux Node.js, a registry entry's Linux
    /// build — which is unpacked inside the distro, never on Windows.
    RuntimeArchive,
}

impl StagedKind {
    /// The copy inside the distro, as the messages name it.
    fn noun(self) -> &'static str {
        match self {
            Self::SandboxHelper => i18n::tc!("wsl sandbox file", "sandbox helper"),
            Self::AgentBinary => i18n::tc!("wsl sandbox file", "agent executable"),
            Self::RuntimeArchive => i18n::tc!("wsl sandbox file", "agent runtime archive"),
        }
    }

    /// The step of copying it, as the messages name it.
    fn copy_step(self) -> &'static str {
        match self {
            Self::SandboxHelper => i18n::tc!("wsl sandbox step", "copying the sandbox helper"),
            Self::AgentBinary => i18n::tc!("wsl sandbox step", "copying the agent executable"),
            Self::RuntimeArchive => i18n::tc!("wsl sandbox step", "copying the agent runtime"),
        }
    }

    /// The source file on the Windows side, as the messages name it.
    fn source_noun(self) -> &'static str {
        match self {
            Self::SandboxHelper => i18n::tc!("wsl sandbox file", "bundled sandbox helper"),
            Self::AgentBinary => i18n::tc!("wsl sandbox file", "agent executable"),
            Self::RuntimeArchive => {
                i18n::tc!("wsl sandbox file", "downloaded agent runtime archive")
            }
        }
    }

    /// What fixes a source file that cannot be read.
    fn source_fix(self) -> &'static str {
        match self {
            Self::SandboxHelper => i18n::t!("Reinstall Zed."),
            Self::AgentBinary => i18n::t!("Download the agent again."),
            Self::RuntimeArchive => i18n::t!("Download it again."),
        }
    }
}

/// SHA-256 of a file as stored, in hex. Cached per `(path, length, modified)`
/// for the life of the process: the artifacts only change when Zed is updated,
/// which restarts it.
async fn file_sha256(path: &Path, kind: StagedKind) -> Result<String> {
    type DigestCache = HashMap<(PathBuf, u64, Option<SystemTime>), String>;
    static CACHE: OnceLock<Mutex<DigestCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    let unreadable = |error: std::io::Error| {
        unavailable(|| {
            i18n::t!(
                "Could not read the {file} `{path}` ({error}). {fix}",
                file = kind.source_noun(),
                path = path.display(),
                fix = kind.source_fix()
            )
        })
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
        unavailable(|| {
            i18n::t!(
                "Could not read the bundled sandbox helper `{path}` ({error}). Reinstall Zed.",
                path = path.display()
            )
        })
    })?;
    let mut decoder = Box::pin(async_compression::futures::bufread::GzipDecoder::new(
        futures::io::BufReader::new(file),
    ));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        let read = decoder.read(&mut buffer).await.map_err(|error| {
            unavailable(|| {
                i18n::t!(
                    "The bundled sandbox helper `{path}` is damaged ({error}). Reinstall Zed.",
                    path = path.display()
                )
            })
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
        HelperCheck::Missing => Err(unavailable(|| {
            i18n::t!(
                "The sandbox helper was installed in {distro}, but it could not be verified. Try again.",
                distro = wsl_distro_label(distro)
            )
        })),
    }
}

fn unverifiable_helper(distro: Option<&str>) -> anyhow::Error {
    unavailable(|| {
        i18n::t!(
            "`sha256sum` is missing in {distro}, so it cannot be verified that the sandbox helper \
         has not been replaced. Install coreutils and try again.",
            distro = wsl_distro_label(distro)
        )
    })
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
        i18n::tc!("wsl sandbox step", "checking the sandbox helper"),
    )
    .await?;
    if !output.status.success() {
        return Err(unavailable(|| {
            i18n::t!(
                "Could not check the sandbox helper in {distro}{details}. \
             Run `wsl --update` and try again.",
                distro = wsl_distro_label(distro),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        }));
    }
    parse_check_output(&String::from_utf8_lossy(&output.stdout)).map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not check the sandbox helper in {distro} ({error:#}).",
                distro = wsl_distro_label(distro)
            )
        })
    })
}

fn parse_check_output(stdout: &str) -> Result<HelperCheck> {
    parse_check_result(stdout, HELPER_RESULT_PREFIX)
}

/// The last line of `stdout` that starts with `prefix`, read as
/// `ready <path>`, `missing <path>` or `unverifiable <path>`.
fn parse_check_result(stdout: &str, prefix: &str) -> Result<HelperCheck> {
    let line = stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(prefix))
        .with_context(|| format!("no check result in {stdout:?}"))?;
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

/// A file copied into its directory under a temporary name, waiting to be
/// moved into place ([`FINALIZE_SCRIPT`], [`AGENT_FINALIZE_SCRIPT`]).
struct StagedFile {
    name: String,
    path: String,
    size: u64,
    /// SHA-256 of what was sent, which is also what arrived.
    sha256: String,
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
    let directory = helper_install_directory(key);
    let mut staged = Vec::with_capacity(files.len());
    for (name, source, gzip) in files {
        match stage_file(
            wsl_exe,
            distro,
            &directory,
            name,
            source,
            gzip,
            StagedKind::SandboxHelper,
        )
        .await
        {
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
    let output = run_wsl_command(
        wsl_exe,
        distro,
        &args,
        i18n::tc!("wsl sandbox step", "installing the sandbox helper"),
    )
    .await?;
    if !output.status.success() {
        discard_staged_files(
            wsl_exe,
            distro,
            staged.iter().map(|file| file.path.as_str()),
        )
        .await;
        return Err(copy_failure(distro, &output, StagedKind::SandboxHelper));
    }
    log::info!(
        "installed the WSL sandbox helper {key} into {}",
        wsl_distro_label(distro)
    );
    Ok(())
}

/// Copy `source` (decompressed when `gzip`) into `directory`, relative to the
/// distro account's home, under a temporary name, and check that what arrived
/// is exactly what was sent.
async fn stage_file(
    wsl_exe: &Path,
    distro: Option<&str>,
    directory: &str,
    name: &str,
    source: &Path,
    gzip: bool,
    kind: StagedKind,
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
            directory,
            name,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command
        .spawn()
        .map_err(|error| wsl_start_failure(kind.copy_step(), &error))?;
    let stdin = child
        .stdin
        .take()
        .context("bug: wsl.exe was spawned without a stdin pipe")?;
    let (sent, output) = futures::join!(send_file(source, gzip, stdin, kind), child.output());
    let output = output.map_err(|error| wsl_start_failure(kind.copy_step(), &error))?;
    let report = parse_stage_output(&String::from_utf8_lossy(&output.stdout));

    // A failed `wsl.exe` is the better explanation of a failed send (the pipe
    // broke because the other end went away), so it is checked first.
    let failure = if !output.status.success() {
        Some(copy_failure(distro, &output, kind))
    } else {
        match (&sent, &report) {
            (Err(_), _) => None,
            (Ok(_), Err(error)) => Some(unavailable(|| {
                i18n::t!(
                    "Could not copy the {file} to {distro} ({error:#}).",
                    file = kind.noun(),
                    distro = wsl_distro_label(distro)
                )
            })),
            (Ok(sent), Ok(report)) => {
                let intact = report.size == sent.size
                    && report
                        .sha256
                        .as_deref()
                        .is_none_or(|received| received == sent.sha256);
                (!intact).then(|| {
                    unavailable(|| i18n::t!(
                        "The {file} `{name}` copied to {distro} does not match the original \
                         ({sent} bytes sent, {received} bytes received). Try again. If that does not \
                         help, check the free space in WSL.",
                        distro = wsl_distro_label(distro),
                        file = kind.noun(),
                        sent = sent.size,
                        received = report.size
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
        name: name.to_string(),
        path: report.path,
        size: sent.size,
        sha256: sent.sha256,
    })
}

/// Stream `source` (decompressed when `gzip`) into `sink`, returning the size
/// and SHA-256 of what was sent.
async fn send_file(
    source: &Path,
    gzip: bool,
    sink: smol::process::ChildStdin,
    kind: StagedKind,
) -> Result<SentFile> {
    let file = smol::fs::File::open(source).await.map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not read the {file} `{path}` ({error}). {fix}",
                file = kind.source_noun(),
                path = source.display(),
                fix = kind.source_fix()
            )
        })
    })?;
    let reader = futures::io::BufReader::new(file);
    if gzip {
        let decoder = async_compression::futures::bufread::GzipDecoder::new(reader);
        pump(Box::pin(decoder), source, sink, kind).await
    } else {
        pump(reader, source, sink, kind).await
    }
}

async fn pump(
    mut reader: impl AsyncRead + Unpin,
    source: &Path,
    mut sink: smol::process::ChildStdin,
    kind: StagedKind,
) -> Result<SentFile> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0; COPY_BUFFER_SIZE];
    loop {
        // A read error here is the artifact itself (a truncated download, a
        // gzip checksum mismatch), so it names the file and the fix.
        let read = reader.read(&mut buffer).await.map_err(|error| {
            unavailable(|| {
                i18n::t!(
                    "The {file} `{path}` is damaged ({error}). {fix}",
                    file = kind.source_noun(),
                    path = source.display(),
                    fix = kind.source_fix()
                )
            })
        })?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).context("bug: read past the buffer")?;
        hasher.update(chunk);
        size += read as u64;
        sink.write_all(chunk)
            .await
            .with_context(|| format!("writing {} into WSL", source.display()))?;
    }
    sink.close()
        .await
        .with_context(|| format!("finishing the copy of {} into WSL", source.display()))?;
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
    WslRuntime::real(wsl_exe, distro)
        .discard_staged(paths)
        .await
}

fn copy_failure(
    distro: Option<&str>,
    output: &std::process::Output,
    kind: StagedKind,
) -> anyhow::Error {
    unavailable(|| {
        i18n::t!(
            "Could not copy the {file} to {distro}{details}. Check the free space in WSL and \
         that the home directory is writable.",
            file = kind.noun(),
            distro = wsl_distro_label(distro),
            details = command_failure_details(output.status.code(), &output.stderr)
        )
    })
}

// ===========================================================================
// ADR-F49: agent binaries and the IDE bridge
// ===========================================================================

/// Where this build's helper is installed, relative to the distro account's
/// home.
fn helper_install_directory(key: &str) -> String {
    format!("{WSL_SANDBOX_HELPER_HOME_RELATIVE}/sandbox/{key}")
}

/// `wsl.exe`, or the environment error explaining that WSL is missing.
fn installed_wsl_exe() -> Result<PathBuf> {
    let wsl_exe = wsl_exe_path();
    if wsl_exe.is_file() {
        Ok(wsl_exe)
    } else {
        Err(unavailable(|| {
            registration_message(&DistroRegistration::NotInstalled).unwrap_or_default()
        }))
    }
}

/// A bad request unless `path` is an absolute Linux path.
fn ensure_linux_absolute_path(path: &str, description: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && !path.contains('\0'),
        i18n::t!("The {description} is not an absolute Linux path (internal error): {path:?}")
    );
    Ok(())
}

/// Prefix of [`AGENT_CHECK_SCRIPT`]'s result line.
const AGENT_RESULT_PREFIX: &str = "zed-wsl-agent:";

/// Reports whether an agent binary is staged. `$1` is the agents directory
/// relative to `$HOME`, `$2` the binary's SHA-256 (the name of its own
/// directory), `$3` its name. It counts only while it still hashes to `$2`;
/// checking refreshes its directory's timestamp, which keeps another build's
/// clean-up (see [`AGENT_FINALIZE_SCRIPT`]) away from it.
const AGENT_CHECK_SCRIPT: &str = r#"
set -u
dir="$HOME/$1/$2"
file="$dir/$3"
if [ -f "$file" ] && [ -x "$file" ]; then
    if ! command -v sha256sum >/dev/null 2>&1; then
        printf 'zed-wsl-agent: unverifiable %s\n' "$file"
        exit 0
    fi
    sum=$(sha256sum < "$file")
    if [ "${sum%% *}" = "$2" ]; then
        touch "$dir"
        printf 'zed-wsl-agent: ready %s\n' "$file"
        exit 0
    fi
fi
printf 'zed-wsl-agent: missing %s\n' "$file"
"#;

/// Moves a staged agent binary into place: `$1` is the agents directory
/// relative to `$HOME`, `$2` the binary's SHA-256, `$3` the staged file, `$4`
/// its name. Then removes the other agent binaries nobody checked for a day,
/// and what interrupted copies left behind.
const AGENT_FINALIZE_SCRIPT: &str = r#"
set -eu
root="$HOME/$1"
dir="$root/$2"
chmod 755 "$3"
mv -f "$3" "$dir/$4"
touch "$dir"
if command -v find >/dev/null 2>&1; then
    find "$root" -mindepth 1 -maxdepth 1 -type d ! -name "$2" -mmin +1440 -exec rm -rf {} + 2>/dev/null || true
    find "$dir" -maxdepth 1 -type f -name '.*' -mmin +60 -exec rm -f {} + 2>/dev/null || true
fi
"#;

/// ADR-F49: copy an agent binary — the Linux Claude Code CLI, say — from
/// Windows into `distro` (`None`: the default distro) and return its absolute
/// path there.
///
/// `source_windows_path` must hash to `expected_sha256`: that is checked before
/// anything is copied, and again, inside WSL, on what arrives. The binary lands
/// in `~/.cache/forgekairos/agents/<sha256>/<name>` with mode 755, moved
/// into place only once complete. That is inside the directory the helper
/// protects, so it is read-only from inside the sandbox. A binary already
/// staged with that digest is reused.
///
/// To run it in the sandbox, pass the returned path as the program
/// (`CommandAndArgs::program`) of the command to wrap; the helper `exec`s an
/// absolute path as it is. Never look it up on `PATH`: inside WSL `PATH`
/// reaches the Windows drives, where a `claude` shim would start a Windows
/// program through interop. Stage it in the distro the session runs in (see
/// [`wsl_distro_for_path`]). Like [`wrap_invocation`], this runs `wsl.exe` and
/// can take seconds while WSL starts.
pub async fn stage_agent_binary(
    distro: Option<&str>,
    name: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
) -> Result<String> {
    ensure_agent_binary_name(name)?;
    let digest = normalized_sha256(expected_sha256)?;
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    match check_agent_binary(&wsl_exe, distro, name, &digest).await? {
        HelperCheck::Ready(path) => return Ok(path),
        HelperCheck::Unverifiable => return Err(unverifiable_agent_binary(distro)),
        HelperCheck::Missing => {}
    }
    // Before copying, so that a file that is not the expected one is refused
    // without sending any of it.
    let source_digest = file_sha256(source_windows_path, StagedKind::AgentBinary).await?;
    ensure!(
        source_digest == digest,
        "`{}` does not have the expected SHA-256 {digest} (it has {source_digest})",
        source_windows_path.display()
    );
    let directory = format!("{AGENT_BINARIES_HOME_RELATIVE}/{digest}");
    let staged = stage_file(
        &wsl_exe,
        distro,
        &directory,
        name,
        source_windows_path,
        false,
        StagedKind::AgentBinary,
    )
    .await?;
    if staged.sha256 != digest {
        discard_staged_files(&wsl_exe, distro, [staged.path.as_str()]).await;
        bail!(
            "`{}` changed while it was being copied into WSL",
            source_windows_path.display()
        );
    }
    let output = run_wsl_command(
        &wsl_exe,
        distro,
        [
            "--exec",
            "sh",
            "-c",
            AGENT_FINALIZE_SCRIPT,
            "zed-wsl-agent-finalize",
            AGENT_BINARIES_HOME_RELATIVE,
            digest.as_str(),
            staged.path.as_str(),
            staged.name.as_str(),
        ],
        i18n::tc!("wsl sandbox step", "installing the agent executable"),
    )
    .await?;
    if !output.status.success() {
        discard_staged_files(&wsl_exe, distro, [staged.path.as_str()]).await;
        return Err(copy_failure(distro, &output, StagedKind::AgentBinary));
    }
    match check_agent_binary(&wsl_exe, distro, name, &digest).await? {
        HelperCheck::Ready(path) => Ok(path),
        HelperCheck::Unverifiable => Err(unverifiable_agent_binary(distro)),
        HelperCheck::Missing => Err(unavailable(|| {
            i18n::t!(
                "The agent executable was installed in {distro}, but it could not be verified. Try again.",
                distro = wsl_distro_label(distro)
            )
        })),
    }
}

/// ADR-F49: the absolute path of the agent binary [`stage_agent_binary`]
/// staged in `distro` as `name` with this SHA-256, while it is there and
/// intact; `None` when it has to be staged (again).
pub async fn staged_agent_binary(
    distro: Option<&str>,
    name: &str,
    expected_sha256: &str,
) -> Result<Option<String>> {
    ensure_agent_binary_name(name)?;
    let digest = normalized_sha256(expected_sha256)?;
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    match check_agent_binary(&wsl_exe, distro, name, &digest).await? {
        HelperCheck::Ready(path) => Ok(Some(path)),
        HelperCheck::Missing => Ok(None),
        HelperCheck::Unverifiable => Err(unverifiable_agent_binary(distro)),
    }
}

async fn check_agent_binary(
    wsl_exe: &Path,
    distro: Option<&str>,
    name: &str,
    digest: &str,
) -> Result<HelperCheck> {
    let output = run_wsl_command(
        wsl_exe,
        distro,
        [
            "--exec",
            "sh",
            "-c",
            AGENT_CHECK_SCRIPT,
            "zed-wsl-agent-check",
            AGENT_BINARIES_HOME_RELATIVE,
            digest,
            name,
        ],
        i18n::tc!("wsl sandbox step", "checking the agent executable"),
    )
    .await?;
    if !output.status.success() {
        return Err(unavailable(|| {
            i18n::t!(
                "Could not check the agent executable in {distro}{details}. \
             Run `wsl --update` and try again.",
                distro = wsl_distro_label(distro),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        }));
    }
    parse_check_result(
        &String::from_utf8_lossy(&output.stdout),
        AGENT_RESULT_PREFIX,
    )
    .map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not check the agent executable in {distro} ({error:#}).",
                distro = wsl_distro_label(distro)
            )
        })
    })
}

fn unverifiable_agent_binary(distro: Option<&str>) -> anyhow::Error {
    unavailable(|| {
        i18n::t!(
            "`sha256sum` is missing in {distro}, so it cannot be verified that the agent executable \
         has not been replaced. Install coreutils and try again.",
            distro = wsl_distro_label(distro)
        )
    })
}

/// A bad request unless `name` is a plain file name: letters, digits, `.`,
/// `_` and `-`, not starting with `.`.
fn ensure_agent_binary_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "invalid agent binary name {name:?}"
    );
    Ok(())
}

/// `digest` as 64 lowercase hex digits, or a bad request.
fn normalized_sha256(digest: &str) -> Result<String> {
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256 {digest:?}"
    );
    Ok(digest.to_ascii_lowercase())
}

/// The bridge's socket in its session directory.
const IDE_RENDEZVOUS_SOCKET_NAME: &str = "bridge.sock";

/// ADR-F49: where the IDE bridge of session `session_id` listens in `distro`
/// (`None`: the default distro):
/// `~/.cache/forgekairos/ide/<session_id>/bridge.sock`. That is inside the
/// directory the helper protects in WSL mode, which the helper insists on — it
/// connects to the socket from outside the sandbox. `session_id` may hold
/// letters, digits, `_` and `-`. Pass the result to [`ide_bridge_invocation`]
/// and as `IdeForward::rendezvous_path`.
pub async fn ide_rendezvous_path(distro: Option<&str>, session_id: &str) -> Result<String> {
    ensure!(
        !session_id.is_empty()
            && session_id.len() <= 64
            && session_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid IDE session id {session_id:?}"
    );
    let root = ide_rendezvous_root(distro).await?;
    Ok(format!("{root}/{session_id}/{IDE_RENDEZVOUS_SOCKET_NAME}"))
}

/// Lists what the rendezvous directory (`$1`) holds under the prefix (`$2`)
/// and removes each entry no running bridge — and no helper connecting to one
/// — names on its command line. The directory is entered first and checked to
/// be the one named, so one replaced by a symlink since is left alone.
///
/// **The order matters**: the candidates are listed *before* `/proc` is
/// walked, as the 第 1 段 sweep does. A bridge's process is in `/proc` before
/// it creates its directory, so walking `/proc` first and listing afterwards
/// would take in a session that started in between — present in the listing,
/// absent from the process list — and remove a rendezvous that is in use.
/// This way a session that started during the sweep is simply not a candidate.
/// Each name is checked again right before it is removed, so one that went
/// away or was replaced in the meantime is left alone.
const SWEEP_RENDEZVOUS_SCRIPT: &str = r#"set -u
root=$1
prefix=$2
cd -P -- "$root" 2>/dev/null && [ "$(pwd -P)" = "$root" ] || exit 0
candidates=""
for entry in "$prefix"*; do
    [ -d "$entry" ] || continue
    case "$entry" in */*|.*|*[!A-Za-z0-9_-]*) continue ;; esac
    candidates="$candidates $entry"
done
[ -n "$candidates" ] || exit 0
running=""
for cmdline in /proc/[0-9]*/cmdline; do
    arguments=$(tr '\0\n' '  ' < "$cmdline" 2>/dev/null) || continue
    case "$arguments" in
        *--ide-bridge*|*--ide-rendezvous*) running="$running$arguments" ;;
    esac
done
for entry in $candidates; do
    case "$running" in *"$root/$entry/"*) continue ;; esac
    [ -d "$entry" ] || continue
    case "$entry" in "$prefix"*) ;; *) continue ;; esac
    rm -rf -- "./$entry" && printf 'zed-sweep-removed %s\n' "$entry"
done
"#;

const SWEEP_REMOVED_PREFIX: &str = "zed-sweep-removed ";

/// ADR-F49: remove the rendezvous directories under `prefix` whose bridge is
/// gone — what a Zed or a WSL that went down before the bridge could clean up
/// leaves in `~/.cache/forgekairos/ide`. Returns the names removed.
///
/// A directory a running bridge (or a helper splicing to one) names on its
/// command line is left alone, so this is safe to run while other sessions of
/// this Zed or another are up. `prefix` tells one launcher's sessions from
/// another's (`claude-`, `acp-`), and may hold letters, digits, `_` and `-`.
pub async fn sweep_ide_rendezvous(distro: Option<&str>, prefix: &str) -> Result<Vec<String>> {
    ensure!(
        !prefix.is_empty()
            && prefix.len() <= 32
            && prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "invalid IDE session prefix {prefix:?}"
    );
    let root = ide_rendezvous_root(distro).await?;
    let wsl_exe = installed_wsl_exe()?;
    let output = run_wsl_command(
        &wsl_exe,
        distro,
        sweep_rendezvous_arguments(&root, prefix),
        i18n::tc!("wsl sandbox step", "cleaning up after the IDE integration"),
    )
    .await?;
    ensure!(
        output.status.success(),
        "the IDE rendezvous sweep failed{}",
        command_failure_details(output.status.code(), &output.stderr)
    );
    Ok(removed_rendezvous_names(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// The `wsl.exe` arguments that run [`SWEEP_RENDEZVOUS_SCRIPT`] over `root`
/// for `prefix`. The script reads them as `$1` and `$2`, so `zed-ide-sweep`
/// (its `$0`) has to sit between the script and them.
fn sweep_rendezvous_arguments<'a>(root: &'a str, prefix: &'a str) -> [&'a str; 7] {
    [
        "--exec",
        "/bin/sh",
        "-c",
        SWEEP_RENDEZVOUS_SCRIPT,
        "zed-ide-sweep",
        root,
        prefix,
    ]
}

/// The rendezvous directories [`SWEEP_RENDEZVOUS_SCRIPT`] reported it removed;
/// anything else it printed is ignored.
fn removed_rendezvous_names(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix(SWEEP_REMOVED_PREFIX))
        .map(|name| name.trim_end().to_string())
        .collect()
}

/// `~/.cache/forgekairos/ide` in `distro`, where every session's rendezvous
/// directory is.
async fn ide_rendezvous_root(distro: Option<&str>) -> Result<String> {
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    let helper = ensure_sandbox_helper(&wsl_exe, distro).await?;
    let home = match home_from_helper_path(&helper.path) {
        Some(home) => home.to_string(),
        None => query_distro_home(&wsl_exe, distro).await?,
    };
    Ok(format!("{home}/{IDE_RENDEZVOUS_HOME_RELATIVE}"))
}

/// The distro account's home, read off the path of an installed helper
/// (`<home>/.cache/forgekairos/sandbox/<key>/zed-remote-server`); `None`
/// for any other shape, such as a development override.
fn home_from_helper_path(path: &str) -> Option<&str> {
    let directory = path.strip_suffix(HELPER_BINARY_NAME)?.strip_suffix('/')?;
    let (installs, _key) = directory.rsplit_once('/')?;
    installs
        .strip_suffix("/sandbox")?
        .strip_suffix(WSL_SANDBOX_HELPER_HOME_RELATIVE)?
        .strip_suffix('/')
        .filter(|home| home.starts_with('/'))
}

/// Prefix of [`HOME_SCRIPT`]'s result line.
const HOME_RESULT_PREFIX: &str = "zed-wsl-home:";
const HOME_SCRIPT: &str = r#"printf 'zed-wsl-home: %s\n' "$HOME""#;

async fn query_distro_home(wsl_exe: &Path, distro: Option<&str>) -> Result<String> {
    let output = run_wsl_command(
        wsl_exe,
        distro,
        ["--exec", "sh", "-c", HOME_SCRIPT],
        i18n::tc!("wsl sandbox step", "checking the home directory"),
    )
    .await?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(HOME_RESULT_PREFIX)?.strip_prefix(' '))
        .map(|home| home.trim_end_matches('/').to_string())
        .filter(|home| home.starts_with('/'))
        .ok_or_else(|| {
            unavailable(|| {
                i18n::t!(
                    "Could not find the home directory of {distro}{details}.",
                    distro = wsl_distro_label(distro),
                    details = command_failure_details(output.status.code(), &output.stderr)
                )
            })
        })
}

/// ADR-F49: the `wsl.exe` program and arguments that start the IDE bridge in
/// `distro` (`None`: the default distro; use the session's, see
/// [`wsl_distro_for_path`]).
///
/// The bridge listens on `rendezvous_path` (see [`ide_rendezvous_path`]) and
/// multiplexes every connection over its standard I/O for
/// [`crate::spawn_bridge_mux`]. It is this build's helper, started through the
/// same verified start (checked against its SHA-256 first).
///
/// `lock` is a path and the content to write there: a new file, never through a
/// symlink, in a directory of the distro account's reached by a per-component
/// no-symlink walk, created if missing. It is how the Claude Code CLI is told
/// where its IDE is (ADR-F49 decision 4). `None` for a session whose client
/// learns that another way — an external ACP agent is handed the URL and the
/// token in its `mcpServers` (decision 24) — and then no lock file is written
/// and nothing is left to clean up.
///
/// Returns the `wsl.exe` program, its arguments, and a **stdin preamble**: the
/// caller must write the returned bytes to the bridge's stdin *before anything
/// else*, then hand the same stdin to [`crate::spawn_bridge_mux`] (empty
/// without a `lock`, so the same order works either way). The lock content
/// carries the CLI's IDE auth token, so it travels on stdin rather than on the
/// command line, where `/proc/<pid>/cmdline` would leak it (ADR-F49 item 5).
///
/// Spawn it with piped stdin and stdout, `CREATE_NO_WINDOW` and an empty
/// `WSLENV`, as the sandbox's own `wsl.exe` is, and before the session it
/// serves: the helper refuses a rendezvous directory that does not exist yet.
/// [`start_ide_bridge`] does all of that. Its stderr says `zed-ide-bridge:
/// ready` once it listens, or `zed-ide-bridge: <code>: <detail>` before it
/// exits with status 126 when it cannot set up — `session-running` when
/// `exclusive_directory` is given and another bridge, from this Zed or another,
/// holds it. It removes the socket and the lock file, and exits, when its stdin
/// closes. Like [`wrap_invocation`], this may copy the helper into WSL first.
pub async fn ide_bridge_invocation(
    distro: Option<&str>,
    rendezvous_path: &str,
    lock: Option<(&str, &[u8])>,
    exclusive_directory: Option<&str>,
) -> Result<(PathBuf, Vec<String>, Vec<u8>)> {
    ensure_linux_absolute_path(
        rendezvous_path,
        i18n::tc!("wsl sandbox path", "IDE rendezvous socket"),
    )?;
    if let Some((lock_file_path, lock_content)) = lock {
        ensure_linux_absolute_path(
            lock_file_path,
            i18n::tc!("wsl sandbox path", "IDE lock file"),
        )?;
        ensure!(
            lock_content.len() <= crate::MAX_IDE_LOCK_CONTENT,
            i18n::t!(
                "The IDE lock file is larger than {limit} bytes (internal error)",
                limit = crate::MAX_IDE_LOCK_CONTENT
            )
        );
    }
    if let Some(directory) = exclusive_directory {
        ensure_linux_absolute_path(
            directory,
            i18n::tc!("wsl sandbox path", "IDE bridge's own folder"),
        )?;
    }
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    let helper = ensure_sandbox_helper(&wsl_exe, distro).await?;
    let args = ide_bridge_command_line(
        distro,
        &helper,
        rendezvous_path,
        lock.map(|(path, _)| path),
        exclusive_directory,
    );
    let preamble = match lock {
        Some((_, content)) => crate::encode_ide_lock_preamble(content),
        None => Vec::new(),
    };
    Ok((wsl_exe, args, preamble))
}

fn ide_bridge_command_line(
    distro: Option<&str>,
    helper: &InstalledHelper,
    rendezvous_path: &str,
    lock_file_path: Option<&str>,
    exclusive_directory: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(distro) = distro {
        args.extend(["-d".to_string(), distro.to_string()]);
    }
    args.extend(helper_invocation(helper));
    args.extend([
        crate::IDE_BRIDGE_FLAG.to_string(),
        "--rendezvous".to_string(),
        rendezvous_path.to_string(),
    ]);
    if let Some(path) = lock_file_path {
        args.extend(["--lock-file".to_string(), path.to_string()]);
    }
    if let Some(directory) = exclusive_directory {
        args.extend(["--exclusive-directory".to_string(), directory.to_string()]);
    }
    args
}

/// A running IDE bridge (ADR-F49): the `wsl.exe` process and the standard I/O
/// that [`crate::spawn_bridge_mux`] takes apart.
pub struct IdeBridge {
    /// Killed when dropped. Keeping it is what keeps the bridge running; its
    /// stdin closing is what makes the bridge remove its socket and exit.
    pub child: smol::process::Child,
    pub stdin: smol::process::ChildStdin,
    pub stdout: smol::process::ChildStdout,
    /// What the bridge says on stderr from here on. Spawn it on an executor
    /// that lives as long as the bridge: it logs those lines and keeps the
    /// bridge's stderr pipe from filling up and stalling it.
    pub log: futures::future::BoxFuture<'static, ()>,
    /// This bridge's place in [`crate::MAX_IDE_BRIDGES`]. Keep it next to
    /// whatever keeps `child`: dropping it hands the slot to the next bridge,
    /// so a slot dropped early lets more bridges run than the limit says.
    pub slot: crate::IdeBridgeSlot,
}

/// ADR-F49: start the bridge [`ide_bridge_invocation`] described, hand it the
/// stdin preamble, and wait until it says it is listening.
///
/// `ready_timeout` is a timer of [`IDE_BRIDGE_READY_TIMEOUT`] from the caller's
/// executor; when it fires first, the returned error says so and dropping the
/// child kills the `wsl.exe`. The error is [`IdeBridgeSessionRunning`] when
/// another bridge holds the exclusive directory, and otherwise carries the
/// Japanese reason the bridge reported.
///
/// Shared by the sandboxed Claude Code CLI (第 1 段) and the external ACP
/// agents started in a WSL sandbox (決定 24), which differ only in whether
/// they ask for a lock file.
pub async fn start_ide_bridge(
    program: &Path,
    arguments: &[String],
    preamble: &[u8],
    ready_timeout: impl std::future::Future<Output = ()>,
) -> Result<IdeBridge> {
    use crate::ide_bridge::{BridgeHost, wait_for_ide_bridge_ready};
    use smol::process::windows::CommandExt as _;

    const PREAMBLE_FAILED: &str =
        i18n::msg!("Could not pass the connection details to the IDE integration relay");

    // ADR-F49 (new decision): taken before the process is started, and given
    // back by every path out of here that does not return the bridge.
    let Some(slot) = crate::IdeBridgeSlot::take() else {
        log::warn!(
            "refusing an IDE bridge: {} are already running",
            crate::MAX_IDE_BRIDGES
        );
        bail!(i18n::t!(
            "{count} IDE integration relays are already running, so no more can be started. \
             Close the agent threads and terminals you are not using and try again.",
            count = crate::MAX_IDE_BRIDGES
        ));
    };

    let mut command = Command::new(program);
    command
        .args(arguments)
        // `wsl.exe` is a Windows program of the user's own session and needs
        // this process's environment to start at all, so it keeps it; the empty
        // `WSLENV` is what stops any of it crossing into the distro.
        .env("WSLENV", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = command.spawn().context(i18n::t!(
        "Could not start the IDE integration relay (wsl.exe)"
    ))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        bail!("bug: the IDE bridge's standard I/O is not piped");
    };
    stdin
        .write_all(preamble)
        .await
        .context(i18n::tr(PREAMBLE_FAILED))?;
    stdin.flush().await.context(i18n::tr(PREAMBLE_FAILED))?;

    let log = wait_for_ide_bridge_ready(stderr, BridgeHost::WslDistro, ready_timeout).await?;
    Ok(IdeBridge {
        child,
        stdin,
        stdout,
        log,
        slot,
    })
}

/// ADR-F49: the WSL distro a sandboxed command started in `path` runs in: the
/// one a `\\wsl.localhost\<distro>\…` path names, or `None` for the default
/// distro. Start the IDE bridge and stage agent binaries in the distro of the
/// session they serve.
pub fn wsl_distro_for_path(path: &Path) -> Option<String> {
    parse_wsl_unc_path(&path.to_string_lossy())
        .ok()
        .and_then(|path| path.distro)
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
    let output = run_wsl_command(
        wsl_exe,
        distro,
        &args,
        i18n::tc!("wsl sandbox step", "testing the sandbox helper"),
    )
    .await?;
    parse_helper_probe(&String::from_utf8_lossy(&output.stdout)).map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not run the sandbox helper in {distro}{details} ({error:#}). \
             Update WSL with `wsl --update` and try again.",
                distro = wsl_distro_label(distro),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        })
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
        return Some(i18n::t!(
            "The sandbox cannot be created because {distro} cannot create user namespaces ({reason}). \
             Update WSL with `wsl --update`, restart it with `wsl --shutdown`, and try again \
             (WSL 2 is required).",
            distro = wsl_distro_label(distro)
        ));
    }
    if probe.landlock_abi < 1 {
        return Some(i18n::t!(
            "The sandbox cannot be created because the kernel of {distro} does not support Landlock. \
             Update WSL with `wsl --update`, restart it with `wsl --shutdown`, and try again.",
            distro = wsl_distro_label(distro)
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
    let output = run_wsl_command(
        wsl_exe,
        distro,
        &args,
        i18n::tc!("wsl sandbox step", "resolving paths"),
    )
    .await?;
    if !output.status.success() {
        return Err(unavailable(|| {
            i18n::t!(
                "Could not resolve the paths in {distro}{details}. Run `wsl --update` and try again.",
                distro = wsl_distro_label(distro),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        }));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resolved = parse_path_resolution_output(&stdout, mappings.len()).map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not resolve the paths in {distro} ({error:#}).",
                distro = wsl_distro_label(distro)
            )
        })
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
    unavailable(|| {
        i18n::t!(
            "Could not start WSL for {description} ({error}). Run `wsl --update` or restart Windows, \
         then try again."
        )
    })
}

fn command_failure_details(exit_code: Option<i32>, stderr: &[u8]) -> String {
    let stderr = decode_wsl_text(stderr);
    let stderr = stderr.trim();
    let exit_status = match exit_code {
        Some(code) => i18n::t!("exit code {code}"),
        None => i18n::t!("ended by a signal").to_string(),
    };
    if stderr.is_empty() {
        format!(" ({exit_status})")
    } else {
        format!(" ({})", i18n::t!("{exit_status}, error output: {stderr}"))
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
        Some(distro) => i18n::t!("the WSL distro `{distro}`"),
        None => i18n::t!("the default WSL distro").to_string(),
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

/// ADR-F49 第 2b 段: the environment of a command this fork wrote for Linux —
/// an agent's own settings, the `PATH` of the Node.js staged in the distro,
/// the `HOME` inside its configuration folder — sorted by name so the command
/// line is stable.
///
/// Unlike [`forwarded_environment`] nothing is dropped for being a Windows
/// value: none of it came from this process's environment, and `PATH` and
/// `HOME` are exactly what the command needs set. Only the names `setenv(3)`
/// itself refuses (empty, or holding `=`) are skipped; the WSL interop
/// variables are removed by the helper's `--unsetenv` either way.
fn linux_environment<S: BuildHasher>(env: &HashMap<String, String, S>) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = env
        .iter()
        .filter(|(name, _)| !name.is_empty() && !name.contains('='))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    environment.sort();
    environment
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
    // A cwd inside the distro is a directory only WSL can see —
    // `resolve_paths` requires it to be there. (A Windows drive cwd is checked
    // here as well, because a mistyped one would otherwise only fail after the
    // round-trip.)
    let path_string = path.to_string_lossy();
    if let Ok(wsl_path) = parse_distro_native_path(&path_string) {
        return Ok(PathMapping::Wsl(wsl_path));
    }
    ensure!(
        path.is_dir(),
        "Windows sandboxing via WSL can only use an existing directory as cwd: {}",
        path.display()
    );
    map_path_to_wsl(path)
}

fn path_to_wsl(path: &Path) -> Result<PathMapping> {
    let path_string = path.to_string_lossy();
    if let Ok(path) = parse_distro_native_path(&path_string) {
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
    if let Ok(path) = parse_distro_native_path(&path_string) {
        return Ok(PathMapping::Wsl(path));
    }
    map_path_to_wsl(path)
}

/// Map a Windows drive path (`C:\…`, or behind the `\\?\` verbatim prefix) to
/// what the distro sees. Paths on the distro's own filesystem never come here —
/// [`parse_distro_native_path`] takes them first, because Windows can neither
/// follow their symlinks nor stat them without the distro running.
fn map_path_to_wsl(path: &Path) -> Result<PathMapping> {
    let path_string = path.to_string_lossy();
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

/// A location on a distro's own filesystem, which only WSL can see: a
/// Linux-absolute path in whichever distro the command runs in, or (ADR-F49
/// 第 2b 段) a `\\wsl.localhost\<distro>\…` share naming one distro in
/// particular — the shape a worktree on ext4 reaches the sandbox as.
///
/// Its existence and its symlinks are checked inside WSL by [`resolve_paths`],
/// never here: Windows cannot follow a distro's symlinks, and even statting the
/// share needs the distro started.
fn parse_distro_native_path(path: &str) -> Result<WslPath> {
    parse_wsl_absolute_path(path).or_else(|_| parse_wsl_unc_path(path))
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
    let share = crate::util::parse_wsl_share_path(path)
        .with_context(|| format!("path is not a WSL UNC path: {path}"))?;
    Ok(WslPath {
        distro: Some(share.distro),
        path: share.linux_path,
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

// ===========================================================================
// ADR-F49 第 2b 段: the Linux runtime an agent runs in, inside WSL
// ===========================================================================

/// Everything the staging steps below do outside this process: start
/// `wsl.exe` and read what it printed, copy one file into the distro, and run
/// the `npm install` that itself happens inside the sandbox.
///
/// Production is [`RealWsl`], which does nothing but call the functions these
/// methods name. The seam exists for the tests, which put a recording fake
/// behind it and can then check the argument vectors, the order of the round
/// trips and the handling of every answer without a distro — the staging
/// steps were otherwise reachable only through the end-to-end tests, which
/// need a real WSL and a real download.
trait WslInvoker: Send + Sync {
    /// See [`installed_wsl_exe`].
    fn wsl_exe(&self) -> Result<PathBuf>;

    /// See [`ensure_distro_registered`].
    fn ensure_distro_registered(&self, distro: Option<&str>) -> Result<()>;

    /// See [`run_wsl_command`].
    fn run<'a>(
        &'a self,
        wsl_exe: &'a Path,
        distro: Option<&'a str>,
        args: Vec<std::ffi::OsString>,
        description: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<std::process::Output>>;

    /// See [`stage_file`]. Nothing the runtime path copies is compressed on
    /// the way in (only the bundled helper is), so this never gzips.
    fn stage_file<'a>(
        &'a self,
        wsl_exe: &'a Path,
        distro: Option<&'a str>,
        directory: &'a str,
        name: &'a str,
        source: &'a Path,
        kind: StagedKind,
    ) -> futures::future::BoxFuture<'a, Result<StagedFile>>;

    /// See [`run_sandboxed_npm_install`].
    fn install_npm_in_sandbox<'a>(
        &'a self,
        node_directory: &'a str,
        package_spec: &'a str,
        staging: &'a str,
        allowed_hosts: Vec<String>,
        editor_network_allowlist: Option<Vec<String>>,
    ) -> futures::future::BoxFuture<'a, Result<()>>;

    /// What this invoker has already learned about the distros it reaches.
    fn memo(&self) -> &WslRuntimeMemo;
}

/// What is asked of a distro once and then remembered: both answers change
/// only with the distro's own environment, which needs WSL restarted.
#[derive(Default)]
struct WslRuntimeMemo {
    data_directories: MemoizedByDistro,
    architectures: MemoizedByDistro,
}

type MemoizedByDistro = Mutex<HashMap<Option<String>, String>>;

impl WslRuntimeMemo {
    fn data_directory(&self, distro: Option<&str>) -> Option<String> {
        Self::remembered(&self.data_directories, distro)
    }

    fn remember_data_directory(&self, distro: Option<&str>, directory: &str) {
        Self::remember(&self.data_directories, distro, directory);
    }

    fn architecture(&self, distro: Option<&str>) -> Option<String> {
        Self::remembered(&self.architectures, distro)
    }

    fn remember_architecture(&self, distro: Option<&str>, architecture: &str) {
        Self::remember(&self.architectures, distro, architecture);
    }

    fn remembered(memo: &MemoizedByDistro, distro: Option<&str>) -> Option<String> {
        memo.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&distro.map(str::to_string))
            .cloned()
    }

    fn remember(memo: &MemoizedByDistro, distro: Option<&str>, value: &str) {
        memo.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(distro.map(str::to_string), value.to_string());
    }
}

/// The invoker that actually starts `wsl.exe`.
struct RealWsl;

impl WslInvoker for RealWsl {
    fn wsl_exe(&self) -> Result<PathBuf> {
        installed_wsl_exe()
    }

    fn ensure_distro_registered(&self, distro: Option<&str>) -> Result<()> {
        ensure_distro_registered(distro)
    }

    fn run<'a>(
        &'a self,
        wsl_exe: &'a Path,
        distro: Option<&'a str>,
        args: Vec<std::ffi::OsString>,
        description: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<std::process::Output>> {
        Box::pin(async move { run_wsl_command(wsl_exe, distro, args, description).await })
    }

    fn stage_file<'a>(
        &'a self,
        wsl_exe: &'a Path,
        distro: Option<&'a str>,
        directory: &'a str,
        name: &'a str,
        source: &'a Path,
        kind: StagedKind,
    ) -> futures::future::BoxFuture<'a, Result<StagedFile>> {
        Box::pin(
            async move { stage_file(wsl_exe, distro, directory, name, source, false, kind).await },
        )
    }

    fn install_npm_in_sandbox<'a>(
        &'a self,
        node_directory: &'a str,
        package_spec: &'a str,
        staging: &'a str,
        allowed_hosts: Vec<String>,
        editor_network_allowlist: Option<Vec<String>>,
    ) -> futures::future::BoxFuture<'a, Result<()>> {
        Box::pin(run_sandboxed_npm_install(
            node_directory,
            package_spec,
            staging,
            allowed_hosts,
            editor_network_allowlist,
        ))
    }

    /// Memoized for the life of the *process*, not of this value: every entry
    /// point makes its own `RealWsl`, and they all mean the same WSL.
    fn memo(&self) -> &WslRuntimeMemo {
        static MEMO: OnceLock<WslRuntimeMemo> = OnceLock::new();
        MEMO.get_or_init(Default::default)
    }
}

/// One distro and how the steps below reach it: what every entry point
/// resolves before its first round trip.
struct WslRuntime<'a> {
    invoker: &'a dyn WslInvoker,
    wsl_exe: PathBuf,
    distro: Option<&'a str>,
}

impl<'a> WslRuntime<'a> {
    /// The two environment checks an entry point makes before reaching WSL,
    /// in the order it made them before this type existed.
    fn open(invoker: &'a dyn WslInvoker, distro: Option<&'a str>) -> Result<Self> {
        invoker.ensure_distro_registered(distro)?;
        let wsl_exe = invoker.wsl_exe()?;
        Ok(Self {
            invoker,
            wsl_exe,
            distro,
        })
    }

    /// A distro reached the only way the code outside this section reaches
    /// one, for the steps that are shared with it.
    fn real(wsl_exe: &Path, distro: Option<&'a str>) -> Self {
        Self {
            invoker: &RealWsl,
            wsl_exe: wsl_exe.to_path_buf(),
            distro,
        }
    }

    fn label(&self) -> String {
        wsl_distro_label(self.distro)
    }

    fn memo(&self) -> &WslRuntimeMemo {
        self.invoker.memo()
    }

    async fn run(
        &self,
        args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
        description: &str,
    ) -> Result<std::process::Output> {
        let args = args
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect();
        self.invoker
            .run(&self.wsl_exe, self.distro, args, description)
            .await
    }

    async fn stage_file(
        &self,
        directory: &str,
        name: &str,
        source: &Path,
        kind: StagedKind,
    ) -> Result<StagedFile> {
        self.invoker
            .stage_file(&self.wsl_exe, self.distro, directory, name, source, kind)
            .await
    }

    /// Best effort: a leftover is also swept up by the next installation.
    async fn discard_staged<'p>(&self, paths: impl IntoIterator<Item = &'p str>) {
        let mut args = vec!["--exec", "rm", "-f", "--"];
        args.extend(paths);
        if args.len() == 4 {
            return;
        }
        match self
            .run(
                &args,
                i18n::tc!("wsl sandbox step", "removing a partly copied file"),
            )
            .await
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => log::warn!(
                "could not remove a partial WSL sandbox helper copy{}",
                command_failure_details(output.status.code(), &output.stderr)
            ),
            Err(error) => {
                log::warn!("could not remove a partial WSL sandbox helper copy: {error:#}")
            }
        }
    }
}

/// The Linux spelling of this fork's directory inside a data/config/state base
/// directory, and the default data base below a Linux home.
///
/// [`crate::APP_DIRECTORY_NAME`] and `protected_paths`'s
/// `DATA_DIRECTORY_HOME_RELATIVE` hold the same two strings, but each compiled
/// for the platform the crate is built for — and this code, built for Windows,
/// has to name the *distro's* directory, which is the Linux build's. So they
/// are spelled out here and checked against the Windows build's spelling by
/// `the_linux_data_directory_matches_this_fork` below.
const LINUX_APP_DIRECTORY_NAME: &str = branding::CURRENT.app_directory_name_lowercase;
const LINUX_DEFAULT_DATA_HOME_RELATIVE: &str = ".local/share";

/// ADR-F49 決定 22: where an npm install is put together inside the distro,
/// relative to the account's home. Deliberately *beside* the Zed data
/// directory rather than in it: the staging directory has to be writable from
/// inside the sandbox, and everything in `.local/share/zed-jp-enhance` is
/// protected and read-hidden. Path components are compared whole, so this name
/// is not inside that one.
const WSL_STAGING_HOME_RELATIVE: &str = branding::concat_const!(
    ".local/share/",
    branding::CURRENT.app_directory_name_lowercase,
    "-staging"
);

/// Prefix of the result line of the scripts that stage a runtime archive.
const ARCHIVE_RESULT_PREFIX: &str = "zed-wsl-archive:";
/// Prefix of the result line of the scripts that install and read an npm
/// package.
const NPM_RESULT_PREFIX: &str = "zed-wsl-npm:";
/// Prefix of [`CONFIG_DIRECTORY_SCRIPT`]'s result line.
const CONFIG_RESULT_PREFIX: &str = "zed-wsl-config:";
/// Prefix of [`DATA_DIRECTORY_SCRIPT`]'s result line.
const DATA_DIRECTORY_RESULT_PREFIX: &str = "zed-wsl-data:";
/// Prefix of [`ARCHITECTURE_SCRIPT`]'s result line.
const ARCHITECTURE_RESULT_PREFIX: &str = "zed-wsl-arch:";

/// The largest `package.json` this reads out of a distro.
const MAX_PACKAGE_JSON_BYTES: usize = 4 * 1024 * 1024;

/// Reports the distro's Zed data directory the way the sandbox helper inside
/// it derives its own (`app_state_directory_from_environment("XDG_DATA_HOME")`
/// falling back to the home-relative default, ignoring a relative value
/// exactly as `absolute_env_path` does). `$1` is the default base below
/// `$HOME`, `$2` this fork's directory name.
const DATA_DIRECTORY_SCRIPT: &str = r#"
set -u
case "${XDG_DATA_HOME:-}" in
    /*) base=$XDG_DATA_HOME ;;
    *) base="$HOME/$1" ;;
esac
printf 'zed-wsl-data: %s/%s\n' "$base" "$2"
"#;

/// Reports the distro's machine architecture, which decides which Linux build
/// of Node.js and of a registry entry the runtime needs.
const ARCHITECTURE_SCRIPT: &str = r#"printf 'zed-wsl-arch: %s\n' "$(uname -m)""#;

/// Reports whether the runtime directory `$1` is already there and complete.
/// `$3` is `exact` to require the marker in it to be `$2` (an archive's
/// digest, or an npm package specification), or `any` for a runtime whose
/// identity is the place it is in rather than anything it hashes to — a
/// registry archive published without a digest, whose directory name is
/// already taken from its version and URL (ADR-F49 決定 25).
const ARCHIVE_CHECK_SCRIPT: &str = r#"
set -u
dir=$1
if [ -d "$dir" ] && [ -f "$dir/.zed-complete" ]; then
    have=$(cat "$dir/.zed-complete" 2>/dev/null) || have=
    if [ "$3" = any ] || [ "$have" = "$2" ]; then
        printf 'zed-wsl-archive: ready %s\n' "$dir"
        exit 0
    fi
fi
printf 'zed-wsl-archive: missing %s\n' "$dir"
"#;

/// ADR-F49 決定 20: unpacks a staged archive **inside the distro** — Windows
/// would drop the execute bits and turn the symbolic links into copies — into
/// a fresh temporary directory that is then moved into place in one step.
///
/// `$1` is the destination, `$2` the staged archive, `$3` its SHA-256 (checked
/// again here, on what arrived), `$4` `targz` / `tarbz2` / `tarxz`, `$5` `1` to
/// strip the archive's single top-level directory, `$6` a suffix for the
/// temporary names.
///
/// What may come out of the archive is decided before anything is written: no
/// member may be absolute or walk through `..`, and afterwards no symbolic
/// link in the result may resolve outside it. The second check is made on the
/// unpacked tree with `readlink -m`, not on `tar -tv` output, whose column
/// layout differs between `tar` implementations; the first keeps the unpacking
/// itself inside the temporary directory, where that check then runs before
/// anything is moved into place.
///
/// The unpacked tree is then also refused if it holds anything that is not a
/// plain file, a directory or a symbolic link, and every file's set-user-ID and
/// set-group-ID bits are cleared: this runs as the distro's account (root in a
/// stock image) outside every namespace, where `tar` creates device nodes and
/// keeps mode bits — and the result is handed back to a sandboxed agent as a
/// read grant (決定 14), which is the very boundary the read hiding (決定 11)
/// draws. A distribution tarball has none of either, so nothing legitimate is
/// lost; an archive published without a digest (決定 25) cannot use one to
/// read the disk behind the sandbox's back.
const ARCHIVE_EXTRACT_SCRIPT: &str = r#"
set -u
umask 077
dest=$1
staged=$2
digest=$3
mode=$4
strip=$5
suffix=$6
tmp="$dest.tmp-$suffix"
fail() {
    rm -rf "$tmp" 2>/dev/null
    rm -f "$tmp.links" "$staged" 2>/dev/null
    printf 'zed-wsl-archive: error %s\n' "$1"
    exit 1
}
command -v sha256sum >/dev/null 2>&1 || fail no-sha256sum
command -v tar >/dev/null 2>&1 || fail no-tar
command -v readlink >/dev/null 2>&1 || fail no-readlink
command -v find >/dev/null 2>&1 || fail no-find
sum=$(sha256sum < "$staged") || fail cannot-hash
[ "${sum%% *}" = "$digest" ] || fail digest-mismatch
case "$mode" in
    targz) decompress=-z ;;
    tarbz2) decompress=-j ;;
    tarxz) decompress=-J ;;
    *) fail unknown-archive-kind ;;
esac
names=$(tar $decompress -tf "$staged") || fail cannot-list
if printf '%s\n' "$names" | grep -q -e '^/' -e '^\.\./' -e '/\.\./' -e '/\.\.$' -e '^\.\.$'; then
    fail unsafe-entry
fi
parent=${dest%/*}
[ -n "$parent" ] || parent=/
mkdir -p "$parent" || fail cannot-create-parent
rm -rf "$tmp" || fail cannot-clear-temporary
mkdir "$tmp" || fail cannot-create-temporary
if [ "$strip" = 1 ]; then
    tar $decompress -xf "$staged" -C "$tmp" --no-same-owner --strip-components=1 || fail cannot-extract
else
    tar $decompress -xf "$staged" -C "$tmp" --no-same-owner || fail cannot-extract
fi
root=$(readlink -m "$tmp") || fail cannot-resolve-temporary
find "$tmp" -type l > "$tmp.links" || fail cannot-scan
while IFS= read -r link; do
    [ -n "$link" ] || continue
    target=$(readlink -m "$link") || fail cannot-resolve-link
    case "$target" in
        "$root"/*) ;;
        *) fail escaping-link ;;
    esac
done < "$tmp.links"
rm -f "$tmp.links"
special=$(find "$tmp" ! -type d ! -type f ! -type l) || fail cannot-scan-entries
if [ -n "$special" ]; then
    fail special-entry
fi
find "$tmp" -type f -exec chmod u-s,g-s {} + || fail cannot-clear-setid
printf '%s\n' "$digest" > "$tmp/.zed-complete" || fail cannot-write-marker
old=
if [ -e "$dest" ]; then
    old="$dest.old-$suffix"
    rm -rf "$old" || fail cannot-clear-previous
    mv "$dest" "$old" || fail cannot-retire-previous
fi
if ! mv "$tmp" "$dest"; then
    if [ -n "$old" ]; then
        mv "$old" "$dest" 2>/dev/null
    fi
    fail cannot-install
fi
if [ -n "$old" ]; then
    rm -rf "$old" 2>/dev/null
fi
rm -f "$staged"
printf 'zed-wsl-archive: ready %s\n' "$dest"
"#;

/// ADR-F49 第 2b 段: which archive format a runtime archive is in. A `zip` is
/// refused rather than unpacked: a Linux distribution is not shipped as one,
/// and `unzip` is not in a stock WSL image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WslArchiveKind {
    TarGz,
    TarBz2,
    TarXz,
    Zip,
}

impl WslArchiveKind {
    /// The word [`ARCHIVE_EXTRACT_SCRIPT`] takes for it; `None` for a format
    /// the distro cannot unpack.
    fn script_mode(self) -> Option<&'static str> {
        match self {
            Self::TarGz => Some("targz"),
            Self::TarBz2 => Some("tarbz2"),
            Self::TarXz => Some("tarxz"),
            Self::Zip => None,
        }
    }
}

/// A name no two staged runtimes of this Zed share, for the temporary
/// directories a staging step moves into place. Not a secret and not relied on
/// for safety — every temporary name is removed before it is created — just
/// unique enough that two installs at once do not collide.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanoseconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanoseconds,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A bad request unless `relative` names a place below the data directory with
/// plain components: letters, digits, `.`, `_` and `-`, never `.`, `..`, an
/// empty component or an absolute path.
fn ensure_safe_relative_path(relative: &str, description: &str) -> Result<()> {
    ensure!(
        !relative.is_empty() && relative.len() <= 200 && !relative.starts_with('/'),
        i18n::t!(
            "The {description} is not a relative path below the data directory (internal error): {relative:?}"
        )
    );
    for segment in relative.split('/') {
        ensure!(
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
            i18n::t!(
                "The {description} contains a component that cannot be used (internal error): {segment:?} in {relative:?}"
            )
        );
    }
    Ok(())
}

/// The components of a path below a package's own directory, with `.` dropped:
/// what the no-follow walks in [`NPM_READ_SCRIPT`] and
/// [`NPM_EXECUTABLE_SCRIPT`] take one at a time. A bad request for anything
/// absolute or walking through `..`.
fn linux_path_segments(path: &str, description: &str) -> Result<Vec<String>> {
    ensure!(
        !path.is_empty() && !path.starts_with('/') && !path.contains('\0'),
        i18n::t!("The {description} is not a relative path (internal error): {path:?}")
    );
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        ensure!(
            segment != "..",
            i18n::t!("The {description} points outside the package (internal error): {path:?}")
        );
        segments.push(segment.to_string());
    }
    ensure!(
        !segments.is_empty(),
        i18n::t!("The {description} points to nothing (internal error): {path:?}")
    );
    Ok(segments)
}

/// A bad request unless `name` is an npm package name: `name` or `@scope/name`
/// of lowercase letters, digits, `.`, `_`, `-` and at most that one `/`, with
/// no component opening on one of those three (npm refuses `.` and `_` there,
/// and a leading `-` would reach npm's own command line as an option).
fn ensure_npm_package_name(name: &str) -> Result<()> {
    let scoped = name.starts_with('@');
    let body = name.strip_prefix('@').unwrap_or(name);
    ensure!(
        !name.is_empty() && name.len() <= 214 && body.matches('/').count() == usize::from(scoped),
        "invalid npm package name {name:?}"
    );
    for segment in body.split('/') {
        ensure!(
            !segment.is_empty()
                && !segment.starts_with(['.', '_', '-'])
                && segment.bytes().all(|byte| byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')),
            "invalid npm package name {name:?}"
        );
    }
    Ok(())
}

/// A bad request unless `spec` is a package specification npm can be handed on
/// a command line of its own: a package name from the registry, optionally
/// with the version or range this fork bounds it at
/// (`project::agent_server_store::bounded_npm_package_spec`).
///
/// Nothing else is installed here — not an option, a local path, a tarball, a
/// `file:` / `git:` / `npm:` specifier, a `github:owner/repo`, or a URL — all
/// of which npm would take from somewhere other than the registry the sandbox
/// lets it reach, and some of which would install from a directory the sandbox
/// itself may write.
fn ensure_npm_package_spec(spec: &str) -> Result<()> {
    ensure!(
        !spec.is_empty() && spec.len() <= 256 && !spec.starts_with('-'),
        "invalid npm package specification {spec:?}"
    );
    // The `@` of a scope is the first character, so only a later one opens the
    // version.
    let (name, version) = match spec.rsplit_once('@') {
        Some((name, version)) if !name.is_empty() => (name, Some(version)),
        _ => (spec, None),
    };
    ensure_npm_package_name(name)
        .with_context(|| format!("invalid npm package specification {spec:?}"))?;
    if let Some(version) = version {
        ensure!(
            !version.is_empty()
                && version.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(
                            byte,
                            b'.' | b'-'
                                | b'+'
                                | b' '
                                | b'<'
                                | b'>'
                                | b'='
                                | b'^'
                                | b'~'
                                | b'*'
                                | b'|'
                        )
                }),
            "invalid npm package specification {spec:?}"
        );
    }
    Ok(())
}

/// The value of the last line of `stdout` that starts with `prefix` and a
/// space.
fn parse_prefixed_line<'a>(stdout: &'a str, prefix: &str) -> Option<&'a str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(prefix)?.strip_prefix(' '))
}

/// ADR-F49 第 2b 段 (決定 20): the distro's Zed data directory, derived exactly
/// as the sandbox helper inside it derives its own — `$XDG_DATA_HOME`, or
/// `$HOME/.local/share`, plus this fork's directory name — so that what is
/// staged below it is what the helper's `--readable` check (決定 14) may show a
/// sandboxed agent, and what its protection (決定 8 / 11) keeps every
/// sandboxed command from writing or reading otherwise.
///
/// Memoized per distro for the life of the process: it changes only with the
/// distro's own environment, which needs WSL restarted. Like
/// [`wrap_invocation`] this runs `wsl.exe` and can take seconds while WSL
/// starts.
pub async fn wsl_agent_runtime_data_directory(distro: Option<&str>) -> Result<String> {
    runtime_data_directory(&RealWsl, distro).await
}

async fn runtime_data_directory(invoker: &dyn WslInvoker, distro: Option<&str>) -> Result<String> {
    // Before the environment checks, so a distro whose answer is already known
    // is not looked up in the registry again either.
    if let Some(directory) = invoker.memo().data_directory(distro) {
        return Ok(directory);
    }

    let runtime = WslRuntime::open(invoker, distro)?;
    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                DATA_DIRECTORY_SCRIPT,
                "zed-wsl-data",
                LINUX_DEFAULT_DATA_HOME_RELATIVE,
                LINUX_APP_DIRECTORY_NAME,
            ],
            i18n::tc!("wsl sandbox step", "checking Zed's data folder"),
        )
        .await?;
    let directory = parse_prefixed_line(
        &String::from_utf8_lossy(&output.stdout),
        DATA_DIRECTORY_RESULT_PREFIX,
    )
    .map(|directory| directory.trim_end_matches('/').to_string())
    .filter(|directory| directory.starts_with('/'))
    .ok_or_else(|| {
        unavailable(|| {
            i18n::t!(
                "Could not find Zed's data folder in {distro}{details}.",
                distro = runtime.label(),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        })
    })?;
    runtime.memo().remember_data_directory(distro, &directory);
    Ok(directory)
}

/// ADR-F49 第 2b 段: the distro's machine architecture (`uname -m`), which
/// decides which Linux build of Node.js and of a registry entry belongs in it
/// — never this Windows process's own. Memoized per distro.
pub async fn wsl_machine_architecture(distro: Option<&str>) -> Result<String> {
    machine_architecture(&RealWsl, distro).await
}

async fn machine_architecture(invoker: &dyn WslInvoker, distro: Option<&str>) -> Result<String> {
    if let Some(architecture) = invoker.memo().architecture(distro) {
        return Ok(architecture);
    }

    let runtime = WslRuntime::open(invoker, distro)?;
    let output = runtime
        .run(
            ["--exec", "sh", "-c", ARCHITECTURE_SCRIPT],
            i18n::tc!("wsl sandbox step", "checking the WSL architecture"),
        )
        .await?;
    let architecture = parse_prefixed_line(
        &String::from_utf8_lossy(&output.stdout),
        ARCHITECTURE_RESULT_PREFIX,
    )
    .map(str::to_string)
    .filter(|architecture| {
        !architecture.is_empty()
            && architecture
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    })
    .ok_or_else(|| {
        unavailable(|| {
            i18n::t!(
                "Could not find the architecture of {distro}{details}.",
                distro = runtime.label(),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        })
    })?;
    runtime.memo().remember_architecture(distro, &architecture);
    Ok(architecture)
}

/// ADR-F49 決定 20 / 21: put the runtime archive at `source_windows_path` —
/// the Linux Node.js, a registry entry's Linux build — into the distro's Zed
/// data directory at `destination_relative_to_data_dir`, and return where it
/// ended up.
///
/// The archive is checked against `expected_sha256` on Windows before any of it
/// is sent, and again inside the distro on what arrived. It is then **unpacked
/// inside the distro**: unpacking on Windows would drop the execute bits and
/// turn the symbolic links into copies. What may come out of it is decided
/// first — no member may be absolute or walk through `..` — and the result is
/// refused if any link in it resolves outside itself. Only then is it moved
/// into place, in one step, behind a marker holding the digest; an
/// installation already there under the same digest is reused untouched.
///
/// `strip_top_level_directory` drops the single directory a distribution
/// tarball wraps everything in (`node-v24.11.0-linux-x64/`), so its contents
/// land directly in the destination.
///
/// The destination is below the directory the helper protects and hides
/// (決定 8 / 11), so a sandboxed command can neither write it nor read it
/// unless that command names it among its readable paths (決定 14). The
/// unpacking itself runs outside the sandbox, but it only unpacks data — none
/// of the archive's code is run. Like [`wrap_invocation`] this runs `wsl.exe`,
/// and the work is minutes rather than seconds.
pub async fn stage_wsl_runtime_archive(
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
    kind: WslArchiveKind,
    strip_top_level_directory: bool,
) -> Result<String> {
    stage_runtime_archive(
        &RealWsl,
        distro,
        destination_relative_to_data_dir,
        source_windows_path,
        expected_sha256,
        kind,
        strip_top_level_directory,
    )
    .await
}

async fn stage_runtime_archive(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
    kind: WslArchiveKind,
    strip_top_level_directory: bool,
) -> Result<String> {
    ensure_safe_relative_path(
        destination_relative_to_data_dir,
        i18n::tc!("wsl sandbox path", "runtime destination"),
    )?;
    let digest = normalized_sha256(expected_sha256)?;
    let Some(mode) = kind.script_mode() else {
        bail!(i18n::t!(
            "A zip archive cannot be extracted in the WSL sandbox (the distro has no `unzip`). \
             Specify a Linux archive in a format such as tar.gz."
        ));
    };
    let runtime = WslRuntime::open(invoker, distro)?;
    let data_directory = runtime_data_directory(invoker, distro).await?;
    let destination = format!("{data_directory}/{destination_relative_to_data_dir}");

    if let HelperCheck::Ready(path) =
        check_wsl_runtime(&runtime, &destination, Some(&digest)).await?
    {
        return Ok(path);
    }

    // Before a byte is sent, so an archive that is not the expected one is
    // refused without copying it into the distro at all.
    let source_digest = file_sha256(source_windows_path, StagedKind::RuntimeArchive).await?;
    ensure!(
        source_digest == digest,
        "`{}` does not have the expected SHA-256 {digest} (it has {source_digest})",
        source_windows_path.display()
    );

    let suffix = unique_suffix();
    // Staged beside where it will be unpacked, so the unpacked tree is moved
    // into place within one filesystem.
    let staging_directory = destination
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_else(|| data_directory.clone());
    let staged = runtime
        .stage_file(
            &staging_directory,
            "runtime-archive",
            source_windows_path,
            StagedKind::RuntimeArchive,
        )
        .await?;
    if staged.sha256 != digest {
        runtime.discard_staged([staged.path.as_str()]).await;
        bail!(
            "`{}` changed while it was being copied into WSL",
            source_windows_path.display()
        );
    }

    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                ARCHIVE_EXTRACT_SCRIPT,
                "zed-wsl-archive-extract",
                destination.as_str(),
                staged.path.as_str(),
                digest.as_str(),
                mode,
                if strip_top_level_directory { "1" } else { "0" },
                suffix.as_str(),
            ],
            i18n::tc!("wsl sandbox step", "extracting the agent runtime"),
        )
        .await?;
    if !output.status.success() {
        runtime.discard_staged([staged.path.as_str()]).await;
        return Err(runtime_script_failure(
            distro,
            &output,
            ARCHIVE_RESULT_PREFIX,
            i18n::t!("Could not extract the agent runtime"),
        ));
    }

    match check_wsl_runtime(&runtime, &destination, Some(&digest)).await? {
        HelperCheck::Ready(path) => Ok(path),
        HelperCheck::Missing | HelperCheck::Unverifiable => Err(unavailable(|| {
            i18n::t!(
                "The agent runtime was extracted in {distro}, but it could not be verified. Try again.",
                distro = runtime.label()
            )
        })),
    }
}

/// Whether the runtime directory `destination` is there and complete for
/// `marker` (an archive's digest, or an npm package specification), or — for
/// `None` — complete under any marker, which is all that can be asked of a
/// runtime whose only identity is the place it is in (ADR-F49 決定 25).
async fn check_wsl_runtime(
    runtime: &WslRuntime<'_>,
    destination: &str,
    marker: Option<&str>,
) -> Result<HelperCheck> {
    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                ARCHIVE_CHECK_SCRIPT,
                "zed-wsl-archive-check",
                destination,
                marker.unwrap_or_default(),
                if marker.is_some() { "exact" } else { "any" },
            ],
            i18n::tc!("wsl sandbox step", "checking the agent runtime"),
        )
        .await?;
    if !output.status.success() {
        return Err(unavailable(|| {
            i18n::t!(
                "Could not check the agent runtime in {distro}{details}.",
                distro = runtime.label(),
                details = command_failure_details(output.status.code(), &output.stderr)
            )
        }));
    }
    parse_check_result(
        &String::from_utf8_lossy(&output.stdout),
        ARCHIVE_RESULT_PREFIX,
    )
    .map_err(|error| {
        unavailable(|| {
            i18n::t!(
                "Could not check the agent runtime in {distro} ({error:#}).",
                distro = runtime.label()
            )
        })
    })
}

/// An error for a runtime script that failed, naming the one-word reason it
/// reported (`zed-wsl-…: error <reason>`) when it got that far.
fn runtime_script_failure(
    distro: Option<&str>,
    output: &std::process::Output,
    prefix: &str,
    what: &str,
) -> anyhow::Error {
    let reason = parse_prefixed_line(&String::from_utf8_lossy(&output.stdout), prefix)
        .and_then(|line| line.strip_prefix("error "))
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    unavailable(|| {
        i18n::t!(
            "{what} in {distro}{reason}{details}.",
            distro = wsl_distro_label(distro),
            details = command_failure_details(output.status.code(), &output.stderr)
        )
    })
}

/// ADR-F49 決定 22: makes the directory an npm install is put together in —
/// the only place the sandboxed npm may write — and the empty npm
/// configuration files that keep the account's own `.npmrc` (and the registry
/// token in it) out of the install. `$1` is the staging root relative to
/// `$HOME`, `$2` a suffix of this install's own.
const NPM_STAGING_SCRIPT: &str = r#"
set -u
umask 077
dir="$HOME/$1/$2"
rm -rf "$dir" 2>/dev/null
mkdir -p "$dir/pkg" "$dir/cache" "$dir/home" || {
    printf 'zed-wsl-npm: error cannot-create-staging\n'
    exit 1
}
: > "$dir/blank_user_npmrc" || {
    printf 'zed-wsl-npm: error cannot-create-npmrc\n'
    exit 1
}
: > "$dir/blank_global_npmrc" || {
    printf 'zed-wsl-npm: error cannot-create-npmrc\n'
    exit 1
}
printf 'zed-wsl-npm: ready %s\n' "$dir"
"#;

/// ADR-F49 決定 22: moves what the sandboxed npm produced out of the staging
/// directory into the protected place the agent is then run from, in one step,
/// and writes the marker last so a half-finished install never counts. `$1` is
/// the staging directory, `$2` the destination, `$3` a suffix for the
/// temporary names, `$4` the package specification to record. The staging
/// directory is removed either way.
const NPM_FINALIZE_SCRIPT: &str = r#"
set -u
umask 077
staging=$1
dest=$2
suffix=$3
marker=$4
tmp="$dest.tmp-$suffix"
fail() {
    rm -rf "$tmp" 2>/dev/null
    rm -rf "$staging" 2>/dev/null
    printf 'zed-wsl-npm: error %s\n' "$1"
    exit 1
}
[ -d "$staging/pkg/node_modules" ] || fail no-node-modules
parent=${dest%/*}
[ -n "$parent" ] || parent=/
mkdir -p "$parent" || fail cannot-create-parent
rm -rf "$tmp" || fail cannot-clear-temporary
mkdir "$tmp" || fail cannot-create-temporary
mv "$staging/pkg/node_modules" "$tmp/node_modules" || fail cannot-move-packages
for name in package.json package-lock.json; do
    if [ -e "$staging/pkg/$name" ]; then
        mv "$staging/pkg/$name" "$tmp/$name" || fail cannot-move-manifest
    fi
done
printf '%s\n' "$marker" > "$tmp/.zed-complete" || fail cannot-write-marker
old=
if [ -e "$dest" ]; then
    old="$dest.old-$suffix"
    rm -rf "$old" || fail cannot-clear-previous
    mv "$dest" "$old" || fail cannot-retire-previous
fi
if ! mv "$tmp" "$dest"; then
    if [ -n "$old" ]; then
        mv "$old" "$dest" 2>/dev/null
    fi
    fail cannot-install
fi
if [ -n "$old" ]; then
    rm -rf "$old" 2>/dev/null
fi
rm -rf "$staging" 2>/dev/null
printf 'zed-wsl-npm: ready %s\n' "$dest"
"#;

/// Reads one file below an installed npm package, following no link at any
/// step and running nothing of the package's. `$1` is the install directory,
/// the rest the path components below it. The file's contents follow the
/// result line.
const NPM_READ_SCRIPT: &str = r#"
set -u
dir=$1
shift
fail() {
    printf 'zed-wsl-npm: error %s\n' "$1"
    exit 1
}
if [ ! -f "$dir/.zed-complete" ]; then
    printf 'zed-wsl-npm: missing %s\n' "$dir"
    exit 0
fi
path=$dir
for name in "$@"; do
    path="$path/$name"
    if [ -L "$path" ]; then
        fail "link $path"
    fi
    if [ ! -e "$path" ]; then
        printf 'zed-wsl-npm: missing %s\n' "$path"
        exit 0
    fi
done
[ -f "$path" ] || fail "not-a-file $path"
printf 'zed-wsl-npm: file %s\n' "$path"
cat "$path" || fail "cannot-read $path"
"#;

/// Confirms an installed npm package's executable is a plain file reached
/// without following a link at any step, and reports its absolute path. `$1`
/// is the install directory, the rest the path components below it.
const NPM_EXECUTABLE_SCRIPT: &str = r#"
set -u
dir=$1
shift
fail() {
    printf 'zed-wsl-npm: error %s\n' "$1"
    exit 1
}
path=$dir
for name in "$@"; do
    path="$path/$name"
    if [ -L "$path" ]; then
        fail "link $path"
    fi
    [ -e "$path" ] || fail "missing $path"
done
[ -f "$path" ] || fail "not-a-file $path"
printf 'zed-wsl-npm: executable %s\n' "$path"
"#;

/// ADR-F49 決定 22: install `package_spec` into the distro's Zed data
/// directory at `install_relative_to_data_dir`, with the Linux npm that came
/// with the Node.js at `node_directory`, and return where it ended up.
///
/// **npm runs inside the sandbox**, because a package's install scripts run
/// with it: it may write only a staging directory of its own, reach only
/// `allowed_hosts` (within `editor_network_allowlist`, the editor-wide ceiling
/// the WSL helper's proxy nests it in), and is given an npm configuration that
/// is empty — its `HOME`, cache and both `.npmrc` files are inside the staging
/// directory, so the account's own registry token is neither used nor
/// reachable. `node_directory` is shown to it read-only.
///
/// What the install produced is then moved into place by the trusted side, in
/// one step, behind a marker holding `package_spec`; the staging directory is
/// removed either way. An install already there for the same specification is
/// reused untouched.
///
/// There is no time limit here: the caller bounds it, and dropping the
/// returned future kills the `wsl.exe` that is running npm.
pub async fn install_npm_package_in_wsl(
    distro: Option<&str>,
    node_directory: &str,
    package_spec: &str,
    install_relative_to_data_dir: &str,
    allowed_hosts: Vec<String>,
    editor_network_allowlist: Option<Vec<String>>,
) -> Result<String> {
    install_npm_package(
        &RealWsl,
        distro,
        node_directory,
        package_spec,
        install_relative_to_data_dir,
        allowed_hosts,
        editor_network_allowlist,
    )
    .await
}

async fn install_npm_package(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    node_directory: &str,
    package_spec: &str,
    install_relative_to_data_dir: &str,
    allowed_hosts: Vec<String>,
    editor_network_allowlist: Option<Vec<String>>,
) -> Result<String> {
    ensure_linux_absolute_path(
        node_directory,
        i18n::tc!("wsl sandbox path", "Node.js installation folder"),
    )?;
    ensure_npm_package_spec(package_spec)?;
    ensure_safe_relative_path(
        install_relative_to_data_dir,
        i18n::tc!("wsl sandbox path", "npm package destination"),
    )?;
    let runtime = WslRuntime::open(invoker, distro)?;
    let data_directory = runtime_data_directory(invoker, distro).await?;
    let destination = format!("{data_directory}/{install_relative_to_data_dir}");

    if let HelperCheck::Ready(path) =
        check_wsl_runtime(&runtime, &destination, Some(package_spec)).await?
    {
        return Ok(path);
    }

    let suffix = unique_suffix();
    let staging = create_npm_staging_directory(&runtime, &suffix).await?;
    let outcome = runtime
        .invoker
        .install_npm_in_sandbox(
            node_directory,
            package_spec,
            &staging,
            allowed_hosts,
            editor_network_allowlist,
        )
        .await;
    if let Err(error) = outcome {
        remove_wsl_directory(&runtime, &staging).await;
        return Err(error);
    }

    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                NPM_FINALIZE_SCRIPT,
                "zed-wsl-npm-finalize",
                staging.as_str(),
                destination.as_str(),
                suffix.as_str(),
                package_spec,
            ],
            i18n::tc!("wsl sandbox step", "installing the npm package"),
        )
        .await?;
    if !output.status.success() {
        remove_wsl_directory(&runtime, &staging).await;
        return Err(runtime_script_failure(
            distro,
            &output,
            NPM_RESULT_PREFIX,
            i18n::t!("Could not install the npm package"),
        ));
    }

    match check_wsl_runtime(&runtime, &destination, Some(package_spec)).await? {
        HelperCheck::Ready(path) => Ok(path),
        HelperCheck::Missing | HelperCheck::Unverifiable => Err(unavailable(|| {
            i18n::t!(
                "The npm package was installed in {distro}, but it could not be verified. Try again.",
                distro = runtime.label()
            )
        })),
    }
}

async fn create_npm_staging_directory(runtime: &WslRuntime<'_>, suffix: &str) -> Result<String> {
    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                NPM_STAGING_SCRIPT,
                "zed-wsl-npm-staging",
                WSL_STAGING_HOME_RELATIVE,
                suffix,
            ],
            i18n::tc!("wsl sandbox step", "creating the npm working folder"),
        )
        .await?;
    if !output.status.success() {
        return Err(runtime_script_failure(
            runtime.distro,
            &output,
            NPM_RESULT_PREFIX,
            i18n::t!("Could not create the npm working folder"),
        ));
    }
    let staging = parse_prefixed_line(&String::from_utf8_lossy(&output.stdout), NPM_RESULT_PREFIX)
        .and_then(|line| line.strip_prefix("ready "))
        .map(str::to_string)
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| {
            unavailable(|| {
                i18n::t!(
                    "Could not create the npm working folder in {distro}.",
                    distro = runtime.label()
                )
            })
        })?;
    Ok(staging)
}

/// The sandboxed `npm install` itself: everything between the staging
/// directory being there and what it produced being moved into place.
async fn run_sandboxed_npm_install(
    node_directory: &str,
    package_spec: &str,
    staging: &str,
    allowed_hosts: Vec<String>,
    editor_network_allowlist: Option<Vec<String>>,
) -> Result<()> {
    let mut sandbox = crate::Sandbox::new(crate::SandboxPolicy {
        fs: crate::SandboxFsPolicy::Restricted {
            writable_paths: vec![capture_wsl_location(
                staging,
                i18n::tc!("wsl sandbox path", "npm working folder"),
            )?],
            protected_paths: Vec::new(),
            readable_paths: vec![capture_wsl_location(
                node_directory,
                i18n::tc!("wsl sandbox path", "Node.js installation folder"),
            )?],
        },
        network: crate::SandboxNetPolicy::Restricted {
            allowed_domains: allowed_hosts,
        },
    })
    .map_err(anyhow::Error::new)?;
    if let Some(entries) = editor_network_allowlist {
        sandbox.set_editor_network_allowlist(entries);
    }
    // The command and its environment are the Linux ones written out below,
    // not this Windows process's.
    sandbox.set_wsl_linux_environment();

    let package_directory = format!("{staging}/pkg");
    let command = crate::CommandAndArgs {
        program: format!("{node_directory}/bin/node"),
        args: vec![
            format!("{node_directory}/lib/node_modules/npm/bin/npm-cli.js"),
            "install".to_string(),
            package_spec.to_string(),
            "--save-exact".to_string(),
            "--no-audit".to_string(),
            "--no-fund".to_string(),
            "--prefix".to_string(),
            package_directory.clone(),
            "--cache".to_string(),
            format!("{staging}/cache"),
            "--userconfig".to_string(),
            format!("{staging}/blank_user_npmrc"),
            "--globalconfig".to_string(),
            format!("{staging}/blank_global_npmrc"),
        ],
        env: HashMap::from([
            ("HOME".to_string(), format!("{staging}/home")),
            (
                "PATH".to_string(),
                format!("{node_directory}/bin:/usr/local/bin:/usr/bin:/bin"),
            ),
        ]),
        cwd: Some(PathBuf::from(package_directory)),
    };
    let wrapped = sandbox.wrap(&command).await.map_err(anyhow::Error::new)?;

    // `cwd` is a path inside the distro, applied by the helper's `--chdir`;
    // the `wsl.exe` on this side keeps this process's own.
    let output = {
        use smol::process::windows::CommandExt as _;
        let mut child = Command::new(&wrapped.program);
        child
            .args(&wrapped.args)
            .envs(&wrapped.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .creation_flags(CREATE_NO_WINDOW);
        child.output().await.map_err(|error| {
            wsl_start_failure(
                i18n::tc!("wsl sandbox step", "running npm install in the sandbox"),
                &error,
            )
        })?
    };
    drop(sandbox);
    ensure!(
        output.status.success(),
        i18n::t!(
            "Could not run `npm install {package_spec}` in the sandbox{details}",
            details = npm_failure_details(&output)
        )
    );
    Ok(())
}

/// The tail of what a failed npm printed, for the message the user sees.
fn npm_failure_details(output: &std::process::Output) -> String {
    const MAX_DETAIL_CHARACTERS: usize = 800;
    let mut detail = decode_wsl_text(&output.stderr);
    if detail.trim().is_empty() {
        detail = decode_wsl_text(&output.stdout);
    }
    let detail = detail.trim();
    let exit_status = match output.status.code() {
        Some(code) => i18n::t!("exit code {code}"),
        None => i18n::t!("ended by a signal").to_string(),
    };
    if detail.is_empty() {
        return format!(" ({exit_status})");
    }
    let tail: String = if detail.chars().count() > MAX_DETAIL_CHARACTERS {
        let skipped = detail.chars().count() - MAX_DETAIL_CHARACTERS;
        detail.chars().skip(skipped).collect()
    } else {
        detail.to_string()
    };
    format!(" ({})", i18n::t!("{exit_status}, npm output: {tail}"))
}

/// A write grant or a read grant on a path inside the distro, for a command
/// that runs there. The shape is checked here; the object itself is resolved
/// and checked inside WSL when the command is wrapped.
fn capture_wsl_location(path: &str, description: &str) -> Result<crate::HostFilesystemLocation> {
    ensure_linux_absolute_path(path, description)?;
    crate::HostFilesystemLocation::capture(Path::new(path)).with_context(|| {
        i18n::t!("Could not pass the path of the {description} to the sandbox: {path:?}")
    })
}

/// Best effort: a leftover staging directory is also removed by the next
/// install that uses the same name, and holds nothing but a failed install.
async fn remove_wsl_directory(runtime: &WslRuntime<'_>, directory: &str) {
    if !directory.starts_with('/') || directory.contains('\0') {
        return;
    }
    match runtime
        .run(
            ["--exec", "rm", "-rf", "--", directory],
            i18n::tc!("wsl sandbox step", "removing the npm working folder"),
        )
        .await
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => log::warn!(
            "could not remove the WSL npm staging directory {directory}{}",
            command_failure_details(output.status.code(), &output.stderr)
        ),
        Err(error) => {
            log::warn!("could not remove the WSL npm staging directory {directory}: {error:#}")
        }
    }
}

/// ADR-F49 決定 22: the `package.json` of `package_name` as
/// [`install_npm_package_in_wsl`] installed it in `install_directory`, or
/// `None` when nothing complete is installed there.
///
/// Read by the trusted side without following a link at any step and without
/// running anything of the package's — what it declares is then interpreted on
/// this side (`node_runtime::package_json_executable` /
/// `package_json_version`), which is the same rule a local install follows.
pub async fn read_wsl_npm_package_json(
    distro: Option<&str>,
    install_directory: &str,
    package_name: &str,
) -> Result<Option<String>> {
    read_npm_package_json(&RealWsl, distro, install_directory, package_name).await
}

async fn read_npm_package_json(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    install_directory: &str,
    package_name: &str,
) -> Result<Option<String>> {
    ensure_linux_absolute_path(
        install_directory,
        i18n::tc!("wsl sandbox path", "npm package installation folder"),
    )?;
    ensure_npm_package_name(package_name)?;
    let runtime = WslRuntime::open(invoker, distro)?;

    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        NPM_READ_SCRIPT.to_string(),
        "zed-wsl-npm-read".to_string(),
        install_directory.to_string(),
        "node_modules".to_string(),
    ];
    args.extend(linux_path_segments(
        package_name,
        i18n::tc!("wsl sandbox path", "npm package name"),
    )?);
    args.push("package.json".to_string());
    let output = runtime
        .run(
            &args,
            i18n::tc!("wsl sandbox step", "checking the npm package"),
        )
        .await?;
    if !output.status.success() {
        return Err(runtime_script_failure(
            distro,
            &output,
            NPM_RESULT_PREFIX,
            i18n::t!("Could not check the npm package"),
        ));
    }
    ensure!(
        output.stdout.len() <= MAX_PACKAGE_JSON_BYTES,
        "the package.json of {package_name} is larger than {MAX_PACKAGE_JSON_BYTES} bytes"
    );
    let stdout = String::from_utf8(output.stdout)
        .context("the package.json read out of WSL is not UTF-8")?;
    let (status, contents) = stdout.split_once('\n').unwrap_or((stdout.as_str(), ""));
    let status = status
        .strip_prefix(NPM_RESULT_PREFIX)
        .and_then(|status| status.strip_prefix(' '))
        .with_context(|| format!("no npm package result in {stdout:?}"))?;
    if status.starts_with("missing") {
        return Ok(None);
    }
    ensure!(
        status.starts_with("file "),
        "unexpected npm package result {status:?}"
    );
    Ok(Some(contents.to_string()))
}

/// ADR-F49 決定 22: the absolute path, inside the distro, of the executable
/// `executable_relative` of the package `package_name` installed in
/// `install_directory` — confirmed to be a plain file reached without
/// following a link at any step.
///
/// `executable_relative` is what the package's own `package.json` declares,
/// read with [`read_wsl_npm_package_json`] and interpreted on this side.
pub async fn wsl_npm_package_executable(
    distro: Option<&str>,
    install_directory: &str,
    package_name: &str,
    executable_relative: &str,
) -> Result<String> {
    npm_package_executable(
        &RealWsl,
        distro,
        install_directory,
        package_name,
        executable_relative,
    )
    .await
}

async fn npm_package_executable(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    install_directory: &str,
    package_name: &str,
    executable_relative: &str,
) -> Result<String> {
    ensure_linux_absolute_path(
        install_directory,
        i18n::tc!("wsl sandbox path", "npm package installation folder"),
    )?;
    ensure_npm_package_name(package_name)?;
    let runtime = WslRuntime::open(invoker, distro)?;

    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        NPM_EXECUTABLE_SCRIPT.to_string(),
        "zed-wsl-npm-executable".to_string(),
        install_directory.to_string(),
        "node_modules".to_string(),
    ];
    args.extend(linux_path_segments(
        package_name,
        i18n::tc!("wsl sandbox path", "npm package name"),
    )?);
    args.extend(linux_path_segments(
        executable_relative,
        i18n::tc!("wsl sandbox path", "npm package executable"),
    )?);
    let output = runtime
        .run(
            &args,
            i18n::tc!("wsl sandbox step", "checking the npm package executable"),
        )
        .await?;
    if !output.status.success() {
        return Err(runtime_script_failure(
            distro,
            &output,
            NPM_RESULT_PREFIX,
            i18n::t!("Could not check the npm package executable"),
        ));
    }
    parse_prefixed_line(&String::from_utf8_lossy(&output.stdout), NPM_RESULT_PREFIX)
        .and_then(|line| line.strip_prefix("executable "))
        .map(str::to_string)
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| {
            unavailable(|| {
                i18n::t!(
                    "Could not check the npm package executable in {distro}.",
                    distro = runtime.label()
                )
            })
        })
}

/// ADR-F49 決定 15: makes an agent's configuration folder for one project
/// inside the distro. `$1` is the folder relative to `$HOME`; each further
/// argument is `d:<relative directory>` or `f:<file name>` below the folder.
///
/// Every step is created owner-only and refused if it is a symbolic link, or
/// anything but a directory (a plain file, for the `f:` entries): the folder
/// holds the agent's sign-in and the hooks it runs, so a link left on the way
/// to it would put both somewhere this did not choose.
const CONFIG_DIRECTORY_SCRIPT: &str = r#"
set -u
umask 077
fail() {
    printf 'zed-wsl-config: error %s\n' "$1"
    exit 1
}
ensure_dir() {
    ensure_parent=$1
    ensure_relative=$2
    ensure_ifs=$IFS
    IFS='/'
    set -f
    set -- $ensure_relative
    set +f
    IFS=$ensure_ifs
    for ensure_name in "$@"; do
        [ -n "$ensure_name" ] || fail empty-segment
        case "$ensure_name" in
            .|..) fail relative-segment ;;
        esac
        ensure_path="$ensure_parent/$ensure_name"
        if [ -L "$ensure_path" ]; then
            fail "link $ensure_path"
        fi
        if [ ! -e "$ensure_path" ]; then
            mkdir -m 700 "$ensure_path" 2>/dev/null
        fi
        if [ -L "$ensure_path" ]; then
            fail "link $ensure_path"
        fi
        [ -d "$ensure_path" ] || fail "not-a-directory $ensure_path"
        ensure_parent=$ensure_path
    done
    ENSURED=$ensure_parent
}
ensure_file() {
    file_path="$1/$2"
    if [ -L "$file_path" ]; then
        fail "link $file_path"
    fi
    if [ ! -e "$file_path" ]; then
        : > "$file_path" 2>/dev/null
    fi
    if [ -L "$file_path" ]; then
        fail "link $file_path"
    fi
    [ -f "$file_path" ] || fail "not-a-file $file_path"
}
[ -d "$HOME" ] || fail no-home
config_relative=$1
shift
ensure_dir "$HOME" "$config_relative"
config=$ENSURED
for entry in "$@"; do
    case "$entry" in
        d:*) ensure_dir "$config" "${entry#d:}" ;;
        f:*) ensure_file "$config" "${entry#f:}" ;;
        *) fail "bad-entry $entry" ;;
    esac
done
printf 'zed-wsl-config: ready %s\n' "$config"
"#;

/// How long a project key is: `project::agent_sandbox::project_key` takes 8
/// bytes of a SHA-256 and spells them in lowercase hex. Checked here as well,
/// because this crate turns the key into a path inside the distro and cannot
/// call the project crate's own check.
const PROJECT_KEY_LENGTH: usize = 16;

/// ADR-F49 決定 15: make the configuration folder of one agent, for one
/// project, inside the distro, and return it:
/// `$HOME/.forgekairos-claude/projects/<project_key>/<folder_name>`, plus
/// `directories_below` (each relative to the folder) and the names every
/// sandbox protects in a root it may write
/// ([`crate::WORKTREE_PROTECTED_RELATIVE`]: the `.json` ones as files, the
/// rest as directories), exactly as `project::agent_sandbox`'s Unix side makes
/// them on a Unix host.
///
/// Every step is made owner-only and refused if it is a symbolic link or is
/// not a directory of its own, so neither the agent's sign-in nor the hooks it
/// runs can be redirected by something left on the way. An existing folder is
/// left as it is.
///
/// Like [`wrap_invocation`], this runs `wsl.exe` and can take seconds while
/// WSL starts.
pub async fn ensure_wsl_agent_config_dir(
    distro: Option<&str>,
    project_key: &str,
    folder_name: &str,
    directories_below: &[String],
) -> Result<String> {
    ensure!(
        project_key.len() == PROJECT_KEY_LENGTH
            && project_key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        i18n::t!("The project identifier is invalid (internal error): {project_key:?}")
    );
    ensure_safe_relative_path(
        folder_name,
        i18n::tc!("wsl sandbox path", "agent folder name"),
    )?;
    ensure!(
        !folder_name.contains('/'),
        i18n::t!("The agent folder name is not a single name (internal error): {folder_name:?}")
    );
    for directory in directories_below {
        ensure_safe_relative_path(
            directory,
            i18n::tc!("wsl sandbox path", "entry in the agent folder"),
        )?;
    }
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    // The helper's probe is what carries the agents' configuration over from
    // the name builds before the rename gave it, and it only does so while the
    // directory under the current name is not there yet: it has to run before
    // this makes that directory. The command this folder is for needs the
    // helper anyway, which then comes from the cache.
    ensure_sandbox_helper(&wsl_exe, distro).await?;

    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        CONFIG_DIRECTORY_SCRIPT.to_string(),
        "zed-wsl-config".to_string(),
        format!(
            "{}/{project_key}/{folder_name}",
            crate::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE
        ),
    ];
    for directory in directories_below {
        args.push(format!("d:{directory}"));
    }
    for name in crate::WORKTREE_PROTECTED_RELATIVE {
        if name.ends_with(".json") {
            args.push(format!("f:{name}"));
        } else {
            args.push(format!("d:{name}"));
        }
    }

    let output = run_wsl_command(
        &wsl_exe,
        distro,
        &args,
        i18n::tc!("wsl sandbox step", "creating the agent settings folder"),
    )
    .await?;
    if !output.status.success() {
        return Err(runtime_script_failure(
            distro,
            &output,
            CONFIG_RESULT_PREFIX,
            i18n::t!("Could not create the agent settings folder"),
        ));
    }
    parse_prefixed_line(
        &String::from_utf8_lossy(&output.stdout),
        CONFIG_RESULT_PREFIX,
    )
    .and_then(|line| line.strip_prefix("ready "))
    .map(str::to_string)
    .filter(|path| path.starts_with('/'))
    .ok_or_else(|| {
        unavailable(|| {
            i18n::t!(
                "Could not create the agent settings folder in {distro}.",
                distro = wsl_distro_label(distro)
            )
        })
    })
}

/// ADR-F49 決定 25: puts one staged file — a registry entry published as a
/// plain Linux executable rather than an archive — into its own runtime
/// directory. `$1` is the destination directory, `$2` the staged file, `$3`
/// its SHA-256, `$4` the name to give it, `$5` `1` to make it executable, `$6`
/// a suffix for the temporary names.
const FILE_STAGE_SCRIPT: &str = r#"
set -u
umask 077
dest=$1
staged=$2
digest=$3
name=$4
executable=$5
suffix=$6
tmp="$dest.tmp-$suffix"
fail() {
    rm -rf "$tmp" 2>/dev/null
    rm -f "$staged" 2>/dev/null
    printf 'zed-wsl-archive: error %s\n' "$1"
    exit 1
}
command -v sha256sum >/dev/null 2>&1 || fail no-sha256sum
sum=$(sha256sum < "$staged") || fail cannot-hash
[ "${sum%% *}" = "$digest" ] || fail digest-mismatch
parent=${dest%/*}
[ -n "$parent" ] || parent=/
mkdir -p "$parent" || fail cannot-create-parent
rm -rf "$tmp" || fail cannot-clear-temporary
mkdir "$tmp" || fail cannot-create-temporary
mv "$staged" "$tmp/$name" || fail cannot-move
if [ "$executable" = 1 ]; then
    chmod 755 "$tmp/$name" || fail cannot-set-mode
else
    chmod 644 "$tmp/$name" || fail cannot-set-mode
fi
printf '%s\n' "$digest" > "$tmp/.zed-complete" || fail cannot-write-marker
old=
if [ -e "$dest" ]; then
    old="$dest.old-$suffix"
    rm -rf "$old" || fail cannot-clear-previous
    mv "$dest" "$old" || fail cannot-retire-previous
fi
if ! mv "$tmp" "$dest"; then
    if [ -n "$old" ]; then
        mv "$old" "$dest" 2>/dev/null
    fi
    fail cannot-install
fi
if [ -n "$old" ]; then
    rm -rf "$old" 2>/dev/null
fi
printf 'zed-wsl-archive: ready %s\n' "$dest"
"#;

/// ADR-F49 第 2b 段: where a runtime staged at
/// `destination_relative_to_data_dir` for `marker` is in the distro, while it
/// is there and complete; `None` when it has to be staged (again).
///
/// `marker` is what the staging step recorded: an archive's SHA-256, or the
/// npm package specification an install was asked for. `None` asks only that
/// something complete is there, which is what a registry archive published
/// without a digest can be asked (ADR-F49 決定 25): its directory name is
/// already taken from its version and URL, exactly as the local installation's
/// is, so what is there is what that version at that URL unpacked to.
///
/// Checking first means a runtime already in the distro costs one `wsl.exe`
/// round-trip rather than a download.
pub async fn staged_wsl_runtime(
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    marker: Option<&str>,
) -> Result<Option<String>> {
    staged_runtime(&RealWsl, distro, destination_relative_to_data_dir, marker).await
}

async fn staged_runtime(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    marker: Option<&str>,
) -> Result<Option<String>> {
    ensure_safe_relative_path(
        destination_relative_to_data_dir,
        i18n::tc!("wsl sandbox path", "runtime destination"),
    )?;
    let runtime = WslRuntime::open(invoker, distro)?;
    let data_directory = runtime_data_directory(invoker, distro).await?;
    let destination = format!("{data_directory}/{destination_relative_to_data_dir}");
    match check_wsl_runtime(&runtime, &destination, marker).await? {
        HelperCheck::Ready(path) => Ok(Some(path)),
        HelperCheck::Missing | HelperCheck::Unverifiable => Ok(None),
    }
}

/// ADR-F49 決定 25: put a single Linux file — a registry entry published as a
/// plain executable rather than an archive — into the distro's Zed data
/// directory at `destination_relative_to_data_dir`, under `file_name`, and
/// return its absolute path there.
///
/// Checked against `expected_sha256` on Windows before anything is sent and
/// again inside the distro on what arrived, then moved into place in one step
/// behind a marker holding the digest, exactly as
/// [`stage_wsl_runtime_archive`] does for an archive. `executable` gives it
/// mode 755 — it is the program the agent is started as.
pub async fn stage_wsl_runtime_file(
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    file_name: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
    executable: bool,
) -> Result<String> {
    stage_runtime_file(
        &RealWsl,
        distro,
        destination_relative_to_data_dir,
        file_name,
        source_windows_path,
        expected_sha256,
        executable,
    )
    .await
}

async fn stage_runtime_file(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    destination_relative_to_data_dir: &str,
    file_name: &str,
    source_windows_path: &Path,
    expected_sha256: &str,
    executable: bool,
) -> Result<String> {
    ensure_safe_relative_path(
        destination_relative_to_data_dir,
        i18n::tc!("wsl sandbox path", "runtime destination"),
    )?;
    ensure_agent_binary_name(file_name)?;
    let digest = normalized_sha256(expected_sha256)?;
    let runtime = WslRuntime::open(invoker, distro)?;
    let data_directory = runtime_data_directory(invoker, distro).await?;
    let destination = format!("{data_directory}/{destination_relative_to_data_dir}");

    if let HelperCheck::Ready(path) =
        check_wsl_runtime(&runtime, &destination, Some(&digest)).await?
    {
        return Ok(format!("{path}/{file_name}"));
    }

    let source_digest = file_sha256(source_windows_path, StagedKind::RuntimeArchive).await?;
    ensure!(
        source_digest == digest,
        "`{}` does not have the expected SHA-256 {digest} (it has {source_digest})",
        source_windows_path.display()
    );

    let suffix = unique_suffix();
    let staging_directory = destination
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_else(|| data_directory.clone());
    let staged = runtime
        .stage_file(
            &staging_directory,
            "runtime-file",
            source_windows_path,
            StagedKind::RuntimeArchive,
        )
        .await?;
    if staged.sha256 != digest {
        runtime.discard_staged([staged.path.as_str()]).await;
        bail!(
            "`{}` changed while it was being copied into WSL",
            source_windows_path.display()
        );
    }

    let output = runtime
        .run(
            [
                "--exec",
                "sh",
                "-c",
                FILE_STAGE_SCRIPT,
                "zed-wsl-runtime-file",
                destination.as_str(),
                staged.path.as_str(),
                digest.as_str(),
                file_name,
                if executable { "1" } else { "0" },
                suffix.as_str(),
            ],
            i18n::tc!("wsl sandbox step", "installing the agent runtime"),
        )
        .await?;
    if !output.status.success() {
        runtime.discard_staged([staged.path.as_str()]).await;
        return Err(runtime_script_failure(
            distro,
            &output,
            ARCHIVE_RESULT_PREFIX,
            i18n::t!("Could not install the agent runtime"),
        ));
    }

    match check_wsl_runtime(&runtime, &destination, Some(&digest)).await? {
        HelperCheck::Ready(path) => Ok(format!("{path}/{file_name}")),
        HelperCheck::Missing | HelperCheck::Unverifiable => Err(unavailable(|| {
            i18n::t!(
                "The agent runtime was installed in {distro}, but it could not be verified. Try again.",
                distro = runtime.label()
            )
        })),
    }
}

/// ADR-F49 第 2b 段: the absolute path, inside the distro, of `relative` below
/// the staged runtime directory `runtime_directory` — confirmed to be a plain
/// file reached without following a link at any step. What a registry entry's
/// `cmd` (`./bin/agent`) names, after its archive was unpacked.
pub async fn wsl_runtime_executable(
    distro: Option<&str>,
    runtime_directory: &str,
    relative: &str,
) -> Result<String> {
    runtime_executable(&RealWsl, distro, runtime_directory, relative).await
}

async fn runtime_executable(
    invoker: &dyn WslInvoker,
    distro: Option<&str>,
    runtime_directory: &str,
    relative: &str,
) -> Result<String> {
    ensure_linux_absolute_path(
        runtime_directory,
        i18n::tc!("wsl sandbox path", "runtime folder"),
    )?;
    let runtime = WslRuntime::open(invoker, distro)?;
    let mut args = vec![
        "--exec".to_string(),
        "sh".to_string(),
        "-c".to_string(),
        NPM_EXECUTABLE_SCRIPT.to_string(),
        "zed-wsl-runtime-executable".to_string(),
        runtime_directory.to_string(),
    ];
    args.extend(linux_path_segments(
        relative,
        i18n::tc!("wsl sandbox path", "runtime executable"),
    )?);
    let output = runtime
        .run(
            &args,
            i18n::tc!("wsl sandbox step", "checking the agent executable"),
        )
        .await?;
    if !output.status.success() {
        return Err(runtime_script_failure(
            distro,
            &output,
            NPM_RESULT_PREFIX,
            i18n::t!("Could not check the agent executable"),
        ));
    }
    parse_prefixed_line(&String::from_utf8_lossy(&output.stdout), NPM_RESULT_PREFIX)
        .and_then(|line| line.strip_prefix("executable "))
        .map(str::to_string)
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| {
            unavailable(|| {
                i18n::t!(
                    "Could not check the agent executable in {distro}.",
                    distro = runtime.label()
                )
            })
        })
}

// ===========================================================================
// ADR-F49 決定 18: what a WSL sandbox still has running
// ===========================================================================

/// ADR-F49 決定 18 (C): what is still running inside the WSL sandbox of the
/// session `session_id` — the one a command was wrapped for after
/// [`crate::Sandbox::set_sandbox_session_id`].
///
/// The answer comes from **outside** the sandbox: the helper's parent recorded
/// which PID namespace belongs to which session id in a directory no sandboxed
/// command can write, and this runs the very same helper binary as a
/// short-lived utility that reads it. Nothing inside the sandbox can shape the
/// result.
///
/// An empty list means nothing of that session is left — including the case
/// where the sandbox is gone entirely (the helper exited, so the kernel
/// stopped everything in its namespace). A failure to *look* is an error.
pub async fn list_wsl_sandbox_processes(
    distro: Option<&str>,
    session_id: &str,
) -> Result<Vec<crate::SandboxProcess>> {
    crate::validate_sandbox_session_id(session_id)?;
    let (output, _) = run_sandbox_session_subcommand(
        distro,
        crate::SANDBOX_PROCESSES_FLAG,
        session_id,
        Vec::new(),
        i18n::tc!("wsl sandbox step", "listing the processes in the sandbox"),
    )
    .await?;
    crate::decode_sandbox_processes(&output)
}

/// ADR-F49 決定 18 (C): stop exactly the processes of `session_id` that
/// `processes` names, and report how many were signalled. A process whose
/// start time no longer matches the one that was listed is left alone, so a
/// recycled pid is never signalled by mistake.
pub async fn stop_wsl_sandbox_processes(
    distro: Option<&str>,
    session_id: &str,
    processes: &[crate::SandboxProcessKey],
) -> Result<usize> {
    crate::validate_sandbox_session_id(session_id)?;
    let (output, _) = run_sandbox_session_subcommand(
        distro,
        crate::SANDBOX_KILL_FLAG,
        session_id,
        processes.iter().map(|process| process.encode()).collect(),
        i18n::tc!("wsl sandbox step", "stopping the processes in the sandbox"),
    )
    .await?;
    output
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .and_then(|line| line.parse::<usize>().ok())
        .with_context(|| format!("the sandbox stop reported {output:?} instead of a count"))
}

/// Runs the helper's own listing / stop subcommand in `distro` and returns its
/// standard output. The one line it prints when it cannot answer
/// ([`crate::SANDBOX_PROCESSES_FAILURE_MARKER`]) becomes an error rather than
/// an empty answer, which is what would otherwise read as "nothing is left
/// running".
async fn run_sandbox_session_subcommand(
    distro: Option<&str>,
    flag: &str,
    session_id: &str,
    arguments: Vec<String>,
    description: &str,
) -> Result<(String, std::process::ExitStatus)> {
    ensure_distro_registered(distro)?;
    let wsl_exe = installed_wsl_exe()?;
    let helper = ensure_sandbox_helper(&wsl_exe, distro).await?;
    let mut args = helper_invocation(&helper);
    args.extend([flag.to_string(), session_id.to_string()]);
    args.extend(arguments);
    let output = run_wsl_command(&wsl_exe, distro, &args, description).await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if let Some(detail) = stdout
        .lines()
        .find_map(|line| line.strip_prefix(crate::SANDBOX_PROCESSES_FAILURE_MARKER))
    {
        bail!(i18n::t!(
            "Could not handle the processes in the sandbox in {distro}:{detail}",
            distro = wsl_distro_label(distro)
        ));
    }
    ensure!(
        output.status.success(),
        i18n::t!(
            "Could not finish {description} in {distro}{details}",
            distro = wsl_distro_label(distro),
            details = command_failure_details(output.status.code(), &output.stderr)
        )
    );
    Ok((stdout, output.status))
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
            Vec::new(),
            SandboxPermissions {
                network: RemoteSandboxNetwork::Blocked,
                network_ceiling: None,
                allow_fs_write: false,
                interactive: false,
                ide_forward: None,
                linux_environment: false,
                sandbox_session_id: None,
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
            &[],
            &RemoteSandboxNetwork::Hosts(vec!["github.com".to_string()]),
            false,
            Some("/mnt/c/proj"),
            &crate::WslHelperOptions {
                network_ceiling: None,
                slirp_sha256: Some("slirp-digest"),
                environment: &[("PAGER".to_string(), String::new())],
                unset_environment: &WSL_INTEROP_VARIABLES,
                interactive: false,
                ide_forward: None,
                sandbox_session_id: None,
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

    /// The default distro and a path naming it in another case come to the
    /// same name.
    #[test]
    fn a_distro_has_one_registered_name() {
        let distros = [
            registered("{a}", "Ubuntu", Some(2)),
            registered("{b}", "Debian", Some(2)),
        ];
        let name = |default_id, requested| registered_name(&distros, default_id, requested);
        assert_eq!(name(Some("{A}"), None).as_deref(), Some("Ubuntu"));
        assert_eq!(name(Some("{a}"), Some("ubuntu")).as_deref(), Some("Ubuntu"));
        assert_eq!(name(Some("{a}"), Some("DEBIAN")).as_deref(), Some("Debian"));
        assert_eq!(name(Some("{missing}"), None), None);
        assert_eq!(name(None, None), None);
        assert_eq!(name(Some("{a}"), Some("Fedora")), None);
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
    fn parse_wsl_unc_path_folds_the_spellings_of_one_location() {
        for spelling in [
            r"\\wsl.localhost\Ubuntu\root\p",
            r"\\wsl$\Ubuntu\root\p",
            r"\\WSL.LOCALHOST\Ubuntu\root\p",
            r"\\?\UNC\wsl.localhost\Ubuntu\root\p",
            r"\\?\UNC\wsl$\Ubuntu\root\p",
            "//wsl.localhost/Ubuntu/root/p",
            "\\\\wsl.localhost\\Ubuntu\\root\\p\\",
            r"\\wsl.localhost\Ubuntu\root\.\p",
        ] {
            let path = parse_wsl_unc_path(spelling).unwrap_or_else(|_| panic!("{spelling}"));
            assert_eq!(path.distro.as_deref(), Some("Ubuntu"), "{spelling}");
            assert_eq!(path.path, "/root/p", "{spelling}");
        }
    }

    #[test]
    fn parse_wsl_unc_path_rejects_shapes_that_name_something_else() {
        for refused in [
            // Not a WSL share.
            r"\\server\share\project",
            r"\\?\UNC\server\share\project",
            r"\\?\C:\project",
            // No distribution name.
            r"\\wsl.localhost",
            r"\\wsl.localhost\\root",
            // `..` would leave the folder that was shown.
            r"\\wsl.localhost\Ubuntu\root\..\etc",
        ] {
            assert!(parse_wsl_unc_path(refused).is_err(), "{refused}");
        }
        // A bare share is the distribution's root. It parses (it names a
        // distro, which `wsl_distro_for_path` wants) but is not a grant — that
        // is enforced where grants are built.
        let root = parse_wsl_unc_path(r"\\wsl.localhost\Ubuntu").expect("names a distro");
        assert_eq!(root.path, "/");
        assert!(
            crate::HostFilesystemLocation::capture(Path::new(r"\\wsl.localhost\Ubuntu")).is_err()
        );
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
            parse_canonical_grant_output("Ubuntu-24.04\n/home/me/real target\nnative-fs\n", None)
                .expect("valid helper output");
        // The bare Linux path, not a `\\wsl.localhost\...` spelling.
        assert_eq!(resolved.canonical, PathBuf::from("/home/me/real target"));
        assert!(!resolved.on_windows_fs);

        let resolved =
            parse_canonical_grant_output("Ubuntu\n/mnt/c/Users/me/proj\nwindows-fs\n", None)
                .expect("valid helper output");
        assert_eq!(resolved.canonical, PathBuf::from("/mnt/c/Users/me/proj"));
        assert!(resolved.on_windows_fs);
    }

    /// ADR-F49 第 2b 段: a grant named through a distribution's share keeps that
    /// spelling, so the stored canonical still says which distribution it is in.
    #[test]
    fn canonical_grant_output_keeps_the_share_a_grant_was_named_through() {
        let resolved =
            parse_canonical_grant_output("Ubuntu\n/root/real target\nnative-fs\n", Some("Ubuntu"))
                .expect("valid helper output");
        assert_eq!(
            resolved.canonical,
            PathBuf::from(r"\\wsl.localhost\ubuntu\root\real target")
        );
        assert!(!resolved.on_windows_fs);

        // `wsl.exe -d` matches the name case-insensitively, and so does the one
        // spelling grants are stored in, so either casing gives one grant rather
        // than two entries naming one folder.
        let resolved = parse_canonical_grant_output("Ubuntu\n/root/p\nnative-fs\n", Some("UBUNTU"))
            .expect("valid helper output");
        assert_eq!(
            resolved.canonical,
            PathBuf::from(r"\\wsl.localhost\ubuntu\root\p")
        );

        // Landing in another distribution than the one asked for means the
        // grant would be persisted for the wrong filesystem.
        assert!(
            parse_canonical_grant_output("Debian\n/root/p\nnative-fs\n", Some("Ubuntu")).is_err()
        );
    }

    /// ADR-F49 decision 33: a bare Linux-absolute grant (`/x`) names no
    /// distribution, so it means whatever the **session** runs in. Resolving it
    /// in the default distro while the session ran in another one showed the
    /// user a folder in one distribution and let the command write in another.
    #[test]
    fn a_bare_linux_grant_is_resolved_in_the_sessions_distro() {
        let session = WslSessionDistro::named("Debian");
        let distro = grant_distro(None, &session).expect("a bare path takes the session's distro");
        assert_eq!(distro.as_deref(), Some("Debian"));

        // Shown and stored as that distribution's share, so the approval dialog
        // names the distribution the command will really write in.
        let resolved =
            parse_canonical_grant_output("Debian\n/home/me/x\nnative-fs\n", Some("Debian"))
                .expect("valid helper output");
        assert_eq!(
            resolved.canonical,
            PathBuf::from(r"\\wsl.localhost\debian\home\me\x")
        );

        // A session in the default distro pins nothing: Zed does not learn that
        // distro's name, and a bare path there stays a bare path.
        let distro = grant_distro(None, &WslSessionDistro::default_distro())
            .expect("the default distro pins nothing");
        assert_eq!(distro, None);
        let resolved = parse_canonical_grant_output("Ubuntu\n/home/me/x\nnative-fs\n", None)
            .expect("valid helper output");
        assert_eq!(resolved.canonical, PathBuf::from("/home/me/x"));
    }

    /// A grant that names a distribution keeps it — and a session that runs in
    /// another one is a request `select_distro` could never serve, so it is
    /// refused at approval rather than shown to the user and then failed.
    #[test]
    fn a_share_grant_keeps_its_own_distro_and_must_match_the_session() {
        assert_eq!(
            grant_distro(Some("Ubuntu"), &WslSessionDistro::default_distro())
                .expect("a share pins its own distro")
                .as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(
            grant_distro(Some("Ubuntu"), &WslSessionDistro::named("UBUNTU"))
                .expect("one distribution spelled two ways is one distribution")
                .as_deref(),
            Some("Ubuntu")
        );
        let refused = grant_distro(Some("Debian"), &WslSessionDistro::named("Ubuntu"))
            .expect_err("a grant in another distribution than the session's");
        assert!(
            format!("{refused:#}").contains("this session runs in `Ubuntu`"),
            "{refused:#}"
        );
    }

    /// A whole distribution is never the folder the user was shown, however it
    /// is named. Refused before any WSL round-trip, so this test needs none.
    #[test]
    fn a_whole_distribution_is_not_a_grant_however_it_is_spelled() {
        for (requested, session) in [
            ("/", WslSessionDistro::default_distro()),
            ("/", WslSessionDistro::named("Ubuntu")),
            (r"\\wsl.localhost\Ubuntu", WslSessionDistro::named("Ubuntu")),
            (r"\\wsl$\Ubuntu\", WslSessionDistro::default_distro()),
        ] {
            // `ResolvedGrant` is deliberately not `Debug`, so match rather than
            // `expect_err`.
            let Err(refused) = smol::block_on(resolve_canonical_for_grant(
                PathBuf::from(requested),
                session,
            )) else {
                panic!("{requested} names a whole distribution, not a folder in one");
            };
            assert!(
                format!("{refused:#}").contains("a whole WSL distro"),
                "{requested}: {refused:#}"
            );
        }
    }

    /// A canonical the share spelling cannot carry back unchanged fails the
    /// grant instead of being stored as a path naming something else.
    #[test]
    fn canonical_grant_output_refuses_a_canonical_with_no_share_spelling() {
        // `\` is an ordinary character in a Linux file name; spelled into a
        // share it would become a separator, so the stored path would name
        // `/root/a/b` — a folder nobody approved.
        // `ResolvedGrant` is deliberately not `Debug`, so match rather than
        // `expect_err`.
        let Err(refused) =
            parse_canonical_grant_output("Ubuntu\n/root/a\\b\nnative-fs\n", Some("Ubuntu"))
        else {
            panic!("a canonical holding a backslash has no share spelling");
        };
        assert!(
            format!("{refused:#}").contains("Windows spelling"),
            "{refused:#}"
        );

        // The distribution's own root spells a bare share, which
        // `require_windows_grant_shape` refuses: the grant would be approved
        // here and then never usable.
        let Err(refused) = parse_canonical_grant_output("Ubuntu\n/\nnative-fs\n", Some("Ubuntu"))
        else {
            panic!("a grant resolving to the distro root is not a folder in it");
        };
        assert!(format!("{refused:#}").contains("whole root"), "{refused:#}");

        // Both are fine for a grant that was not named through a share: it
        // is stored as the Linux path the helper reported.
        let resolved = parse_canonical_grant_output("Ubuntu\n/root/a\\b\nnative-fs\n", None)
            .expect("a Linux-absolute grant keeps its own spelling");
        assert_eq!(resolved.canonical, PathBuf::from(r"/root/a\b"));
    }

    #[test]
    fn canonical_grant_output_rejects_malformed_protocol() {
        // Missing filesystem-class line.
        assert!(parse_canonical_grant_output("Ubuntu\n/home/me/project\n", None).is_err());
        // Non-absolute canonical path.
        assert!(parse_canonical_grant_output("Ubuntu\nrelative\nnative-fs\n", None).is_err());
        // Distro name containing a separator.
        assert!(
            parse_canonical_grant_output("Ubuntu/bad\n/home/me/project\nnative-fs\n", None)
                .is_err()
        );
        // Distro name that would climb out of the share when spelled as a path.
        assert!(
            parse_canonical_grant_output("..\n/home/me/project\nnative-fs\n", Some("..")).is_err()
        );
        // Unknown filesystem class.
        assert!(
            parse_canonical_grant_output("Ubuntu\n/home/me/project\nmaybe-fs\n", None).is_err()
        );
        // Trailing junk after the filesystem class.
        assert!(
            parse_canonical_grant_output("Ubuntu\n/home/me/project\nnative-fs\nnoise\n", None)
                .is_err()
        );
    }

    #[test]
    fn split_resolved_paths_keeps_existing_protected_paths_and_skips_missing_ones() {
        let (cwd, writable_paths, readable_paths, protected_paths) = split_resolved_paths(
            true,
            1,
            1,
            vec![
                Some("/home/me/project".to_string()),
                Some("/home/me/project".to_string()),
                Some("/home/me/.local/share/zed-jp-enhance/node".to_string()),
                None,
                Some("/mnt/c/external/.git".to_string()),
            ],
        )
        .unwrap();

        assert_eq!(cwd.as_deref(), Some("/home/me/project"));
        assert_eq!(writable_paths, vec!["/home/me/project".to_string()]);
        assert_eq!(
            readable_paths,
            vec!["/home/me/.local/share/zed-jp-enhance/node".to_string()]
        );
        assert_eq!(protected_paths, vec!["/mnt/c/external/.git".to_string()]);
    }

    /// What the command may read has to be there: the helper decides whether
    /// it may be shown from where it really lies.
    #[test]
    fn split_resolved_paths_rejects_missing_readable_paths() {
        let error = split_resolved_paths(false, 0, 1, vec![None]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("The readable path was passed on unresolved"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn split_resolved_paths_rejects_missing_required_writable_paths() {
        let error = split_resolved_paths(false, 1, 0, vec![None]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("The writable path was passed on unresolved"),
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
        let error = unavailable(|| message.clone());
        let typed = error
            .downcast_ref::<WslSandboxUnavailable>()
            .expect("environment failure should downcast to WslSandboxUnavailable");
        assert_eq!(typed.message().display(), message);
        assert!(format!("{error:#}").starts_with(WSL_SANDBOX_UNAVAILABLE_PREFIX));
    }

    /// An environment failure is shown on the agent's retry card and told to the
    /// agent. `acp_thread` prepares the sandbox in English, so the reason keeps
    /// the UI language for the card even when it is built there.
    #[test]
    fn unavailable_errors_carry_english_for_the_agent_and_the_ui_language_for_the_user() {
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let error = i18n::in_english(|| {
            crate::map_anyhow_error(unavailable(|| {
                registration_message(&DistroRegistration::NotInstalled).unwrap_or_default()
            }))
        });
        let crate::SandboxError::WslUnavailable(message) = &error else {
            panic!("expected WslUnavailable, got {error:?}");
        };
        assert!(
            message
                .english()
                .starts_with("Could not create the sandbox in WSL: WSL is not installed."),
            "{}",
            message.english()
        );
        assert!(message.english().is_ascii(), "{}", message.english());
        assert_eq!(
            message.display(),
            format!(
                "{}: {}",
                i18n::tr(WSL_SANDBOX_UNAVAILABLE_PREFIX),
                registration_message(&DistroRegistration::NotInstalled).unwrap_or_default()
            )
        );
        assert!(!message.display().is_ascii(), "{}", message.display());
        assert_eq!(error.to_string(), message.display());
        assert_eq!(i18n::in_english(|| error.to_string()), message.english());
    }

    /// `map_path_to_wsl` maps Windows drives and nothing else. A path on the
    /// distro's own filesystem must be taken by `parse_distro_native_path`
    /// *before* this, because everything here — the caller's existence check
    /// included — asks Windows about a path only the distro can resolve. If a
    /// share ever reached it, failing is the safe answer: mapped as a drive
    /// path it would name something else entirely.
    #[test]
    fn map_path_to_wsl_refuses_a_path_inside_a_distro() {
        for refused in [
            Path::new(r"\\wsl.localhost\Ubuntu\root\p"),
            Path::new(r"\\wsl$\Ubuntu\root\p"),
            Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\root\p"),
            Path::new(r"\\server\share\p"),
        ] {
            assert!(map_path_to_wsl(refused).is_err(), "{}", refused.display());
        }
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

    #[test]
    fn the_bridge_starts_through_the_verified_helper() {
        let helper = InstalledHelper {
            path: "/root/.cache/zed-jp-enhance/sandbox/abc/zed-remote-server".to_string(),
            digests: Some(HelperDigests {
                server: "server-digest".to_string(),
                slirp: "slirp-digest".to_string(),
            }),
        };
        let args = ide_bridge_command_line(
            Some("Ubuntu"),
            &helper,
            "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock",
            Some("/root/.claude-zed/ide/45123.lock"),
            Some("/root/.claude-zed"),
        );
        assert_eq!(
            args,
            [
                "-d",
                "Ubuntu",
                "--exec",
                "/bin/sh",
                "-c",
                VERIFIED_EXEC_SCRIPT,
                "zed-wsl-exec",
                "/root/.cache/zed-jp-enhance/sandbox/abc/zed-remote-server",
                "server-digest",
                "--ide-bridge",
                "--rendezvous",
                "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock",
                "--lock-file",
                "/root/.claude-zed/ide/45123.lock",
                "--exclusive-directory",
                "/root/.claude-zed",
            ],
            "the lock content is not on the command line"
        );
        // ADR-F53: an SSH host's bridge is started from the same flags, so the
        // two spellings cannot drift apart.
        let from_ide_bridge = args
            .iter()
            .position(|argument| argument == crate::IDE_BRIDGE_FLAG)
            .expect("the WSL invocation runs the helper as the bridge");
        assert_eq!(
            args.get(from_ide_bridge..).unwrap_or_default(),
            crate::remote_ide_bridge_argv(
                "/root/.cache/zed-jp-enhance/ide/s1/bridge.sock",
                Some("/root/.claude-zed/ide/45123.lock"),
                Some("/root/.claude-zed"),
            )
            .expect("absolute host paths")
        );
        // The token rides on stdin instead, as a length-prefixed preamble.
        assert_eq!(
            crate::encode_ide_lock_preamble(b"{\"pid\":1}"),
            [&9u32.to_le_bytes()[..], b"{\"pid\":1}"].concat()
        );
        // ADR-F49 decision 24: a client told where its IDE is through
        // `mcpServers` asks for no lock file, and then none is written and
        // none is left behind.
        assert_eq!(
            ide_bridge_command_line(
                None,
                &helper,
                "/root/.cache/zed-jp-enhance/ide/acp-1/bridge.sock",
                None,
                None,
            ),
            [
                "--exec",
                "/bin/sh",
                "-c",
                VERIFIED_EXEC_SCRIPT,
                "zed-wsl-exec",
                "/root/.cache/zed-jp-enhance/sandbox/abc/zed-remote-server",
                "server-digest",
                "--ide-bridge",
                "--rendezvous",
                "/root/.cache/zed-jp-enhance/ide/acp-1/bridge.sock",
            ]
        );
    }

    /// ADR-F49: the sweep lists its candidates **before** it walks `/proc`.
    /// A bridge's process is there before it makes its directory, so the other
    /// order would take in a session that started in between — listed, but not
    /// yet among the running processes — and remove a rendezvous in use. Every
    /// name is checked again right before it is removed.
    #[test]
    fn the_rendezvous_sweep_lists_before_it_walks_proc() {
        let script = SWEEP_RENDEZVOUS_SCRIPT;
        let step = |needle: &str| {
            script
                .find(needle)
                .unwrap_or_else(|| panic!("the sweep no longer does {needle:?}"))
        };
        let listed = step("candidates=\"$candidates $entry\"");
        let walked = step("/proc/[0-9]*/cmdline");
        let removed = step("rm -rf --");
        assert!(
            listed < walked && walked < removed,
            "list the candidates, then walk /proc, then remove: {listed} {walked} {removed}"
        );
        // The re-checks between the process walk and the removal.
        let rechecked = script
            .get(walked..removed)
            .expect("the text between the walk and the removal");
        assert!(rechecked.contains("[ -d \"$entry\" ]"), "{rechecked}");
        assert!(rechecked.contains("\"$prefix\"*"), "{rechecked}");
        // And the directory it works in is the one it was given, not one
        // reached through a symlink planted since.
        assert!(script.contains("[ \"$(pwd -P)\" = \"$root\" ]"));
    }

    /// The script reads the rendezvous root as `$1` and the prefix as `$2`, so
    /// its own `$0` has to sit between them and `-c`.
    #[test]
    fn the_rendezvous_sweep_is_given_its_root_and_prefix_in_order() {
        let arguments = sweep_rendezvous_arguments("/root/.cache/zed-jp-enhance/ide", "acp-");
        assert_eq!(arguments[..3], ["--exec", "/bin/sh", "-c"]);
        assert_eq!(arguments[3], SWEEP_RENDEZVOUS_SCRIPT);
        assert_eq!(
            arguments[4..],
            ["zed-ide-sweep", "/root/.cache/zed-jp-enhance/ide", "acp-"]
        );

        assert_eq!(
            removed_rendezvous_names(
                "zed-sweep-removed acp-1111
something else
zed-sweep-removed acp-2222
"
            ),
            ["acp-1111", "acp-2222"]
        );
        assert!(removed_rendezvous_names("").is_empty());
    }

    #[test]
    fn the_distro_home_is_read_off_an_installed_helper() {
        let cache = WSL_SANDBOX_HELPER_HOME_RELATIVE;
        assert_eq!(
            home_from_helper_path(&format!("/root/{cache}/sandbox/0123abcd/zed-remote-server")),
            Some("/root")
        );
        assert_eq!(
            home_from_helper_path(&format!(
                "/home/my user/{cache}/sandbox/k/zed-remote-server"
            )),
            Some("/home/my user")
        );
        assert_eq!(home_from_helper_path("/opt/zed/remote_server"), None);
        assert_eq!(
            home_from_helper_path(&format!("/{cache}/sandbox/k/zed-remote-server")),
            None
        );
        // What a build from before the rename installed is not this build's.
        assert_eq!(
            home_from_helper_path("/root/.cache/zed-jp-enhance/sandbox/k/zed-remote-server"),
            None
        );
    }

    #[test]
    fn the_distro_of_a_session_is_the_one_its_worktree_names() {
        assert_eq!(
            wsl_distro_for_path(Path::new(r"\\wsl.localhost\Debian\home\me\project")),
            Some("Debian".to_string())
        );
        assert_eq!(wsl_distro_for_path(Path::new(r"C:\Users\me\project")), None);
    }

    #[test]
    fn agent_binary_names_and_digests_are_checked() {
        assert!(ensure_agent_binary_name("claude").is_ok());
        assert!(ensure_agent_binary_name("claude-2.1.0_x86").is_ok());
        for invalid in ["", ".hidden", "a/b", "..", "a b", &"x".repeat(65)] {
            assert!(ensure_agent_binary_name(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            normalized_sha256(&"AB".repeat(32)).expect("valid"),
            "ab".repeat(32)
        );
        for invalid in ["", "ab", &"g".repeat(64), &"a".repeat(65)] {
            assert!(normalized_sha256(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn agent_check_results_are_read_with_their_own_prefix() {
        assert_eq!(
            parse_check_result(
                "zed-wsl-agent: ready /root/.cache/zed-jp-enhance/agents/ab/claude\n",
                AGENT_RESULT_PREFIX
            )
            .expect("valid"),
            HelperCheck::Ready("/root/.cache/zed-jp-enhance/agents/ab/claude".to_string())
        );
        assert_eq!(
            parse_check_result("zed-wsl-agent: missing /x\n", AGENT_RESULT_PREFIX).expect("valid"),
            HelperCheck::Missing
        );
        assert!(parse_check_result("zed-wsl-helper: ready /x\n", AGENT_RESULT_PREFIX).is_err());
        assert!(AGENT_CHECK_SCRIPT.contains("zed-wsl-agent: ready"));
    }

    #[test]
    fn bridge_paths_must_be_absolute_linux_paths() {
        assert!(ensure_linux_absolute_path("/root/x", "test").is_ok());
        for invalid in ["relative", r"C:\x", "", "/a\0b"] {
            assert!(
                ensure_linux_absolute_path(invalid, "test").is_err(),
                "{invalid:?}"
            );
        }
    }

    /// Stages a file into the real default distro, runs it, and reads it back
    /// (ADR-F49). It needs WSL, so it runs only when asked for:
    /// `cargo test -p sandbox --lib -- --ignored agent_binary_staging_round_trip`.
    #[test]
    #[ignore = "needs WSL"]
    fn agent_binary_staging_round_trip() {
        smol::block_on(async {
            let source = tempfile::NamedTempFile::new().expect("temp file");
            let marker = format!("zed-f49s-staged-{}", std::process::id());
            std::fs::write(source.path(), format!("#!/bin/sh\necho {marker}\n")).expect("write");
            let digest =
                hex_string(Sha256::digest(std::fs::read(source.path()).expect("read")).as_slice());
            let name = "zed-f49s-test-agent";

            assert_eq!(
                staged_agent_binary(None, name, &digest)
                    .await
                    .expect("check"),
                None
            );
            assert!(
                stage_agent_binary(None, name, source.path(), &"0".repeat(64))
                    .await
                    .is_err(),
                "a file with another digest is refused"
            );
            let staged = stage_agent_binary(None, name, source.path(), &digest)
                .await
                .expect("stage");
            assert!(
                staged.ends_with(&format!("/{AGENT_BINARIES_HOME_RELATIVE}/{digest}/{name}")),
                "{staged}"
            );
            assert_eq!(
                staged_agent_binary(None, name, &digest)
                    .await
                    .expect("check"),
                Some(staged.clone())
            );
            assert_eq!(
                stage_agent_binary(None, name, source.path(), &digest)
                    .await
                    .expect("reuse"),
                staged
            );

            let wsl_exe = installed_wsl_exe().expect("wsl.exe");
            let output = run_wsl_command(&wsl_exe, None, ["--exec", staged.as_str()], "test")
                .await
                .expect("run it");
            let cleanup = run_wsl_command(
                &wsl_exe,
                None,
                [
                    "--exec",
                    "sh",
                    "-c",
                    r#"rm -rf "$HOME/$1/$2"; rmdir "$HOME/$1" "$HOME/$3" 2>/dev/null; true"#,
                    "zed-f49s-cleanup",
                    AGENT_BINARIES_HOME_RELATIVE,
                    digest.as_str(),
                    WSL_SANDBOX_HELPER_HOME_RELATIVE,
                ],
                "test",
            )
            .await;
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                marker,
                "the staged binary runs"
            );
            assert!(cleanup.expect("clean up").status.success());
            assert_eq!(
                staged_agent_binary(None, name, &digest)
                    .await
                    .expect("check"),
                None
            );
        });
    }

    // =======================================================================
    // ADR-F49 第 2b 段: the Linux runtime inside WSL
    // =======================================================================

    /// The distro's data directory is the **Linux** spelling of this fork's,
    /// which this Windows build cannot read off `APP_DIRECTORY_NAME` (compiled
    /// for Windows) and therefore spells out. If either ever changes, this
    /// fails rather than silently staging the runtime somewhere the helper
    /// neither protects nor may show the agent.
    #[test]
    fn the_linux_data_directory_matches_this_fork() {
        assert_eq!(
            crate::APP_DIRECTORY_NAME.to_ascii_lowercase(),
            LINUX_APP_DIRECTORY_NAME
        );
        assert_eq!(
            format!("{LINUX_DEFAULT_DATA_HOME_RELATIVE}/{LINUX_APP_DIRECTORY_NAME}"),
            format!(
                ".local/share/{}",
                branding::CURRENT.app_directory_name_lowercase
            ),
            "the helper's `remote_agent_runtime_data_directory` default"
        );
        // ADR-F49 決定 22: the staging directory is *beside* the data
        // directory, never inside it — everything inside is protected and
        // read-hidden, and npm has to be able to write where it works.
        assert!(
            !Path::new(WSL_STAGING_HOME_RELATIVE).starts_with(format!(
                "{LINUX_DEFAULT_DATA_HOME_RELATIVE}/{LINUX_APP_DIRECTORY_NAME}"
            )),
            "the npm staging directory must not be inside the Zed data directory"
        );
    }

    /// ADR-F49 決定 14: what is staged for the WSL sandbox lands below the two
    /// directories the helper's `--readable` check lets a command be shown
    /// (`external_agents` and `node` in the data directory).
    #[test]
    fn staged_runtimes_land_where_a_command_may_be_shown_them() {
        for relative in [
            "node/wsl-sandbox/node-v24.11.0-linux-x64",
            "external_agents/wsl-sandbox/registry/npx/claude-acp",
            "external_agents/wsl-sandbox/registry/archive/some-agent/v1",
        ] {
            ensure_safe_relative_path(relative, "runtime destination")
                .unwrap_or_else(|error| panic!("{relative} should be usable: {error}"));
            assert!(
                relative.starts_with("node/") || relative.starts_with("external_agents/"),
                "{relative} is outside the runtime directories"
            );
        }
    }

    #[test]
    fn a_runtime_destination_can_never_leave_the_data_directory() {
        for refused in [
            "",
            "/absolute",
            "..",
            "../elsewhere",
            "node/../../.ssh",
            "node//double",
            "node/./here",
            "node/wsl sandbox",
            "node/wsl;sandbox",
            &"n".repeat(201),
        ] {
            assert!(
                ensure_safe_relative_path(refused, "runtime destination").is_err(),
                "{refused:?} must be refused"
            );
        }
    }

    #[test]
    fn a_package_path_is_walked_one_plain_component_at_a_time() {
        assert_eq!(
            linux_path_segments("@agentclientprotocol/claude-agent-acp", "npm package")
                .expect("a scoped package"),
            vec![
                "@agentclientprotocol".to_string(),
                "claude-agent-acp".to_string()
            ]
        );
        assert_eq!(
            linux_path_segments("./dist/cli.js", "npm package executable").expect("a bin path"),
            vec!["dist".to_string(), "cli.js".to_string()]
        );
        for refused in ["", "/etc/passwd", "../../etc/passwd", "dist/../../x", "."] {
            assert!(
                linux_path_segments(refused, "npm package executable").is_err(),
                "{refused:?} must be refused"
            );
        }
    }

    #[test]
    fn only_a_registry_package_specification_is_installed() {
        for accepted in [
            "@agentclientprotocol/claude-agent-acp@0.76.0",
            "codex-acp@0.0.0 - 1.11.0",
            "@google/gemini-cli",
        ] {
            ensure_npm_package_spec(accepted)
                .unwrap_or_else(|error| panic!("{accepted:?} should be accepted: {error}"));
        }
        for refused in [
            "",
            "--global",
            "./local-tarball.tgz",
            "/tmp/evil.tgz",
            "https://example.com/evil.tgz",
            "git+ssh://example.com/evil.git",
            "pkg@1.0.0\nrm -rf /",
            // npm would take every one of these from somewhere other than the
            // registry — a directory the sandbox may itself write, among them.
            "pkg@file:../staging/pkg",
            "pkg@file:/tmp/evil",
            "github:owner/repo",
            "owner/repo#semver:1.0.0",
            "pkg@npm:other-package",
            "pkg@git+https://example.com/evil.git",
            "pkg@https://example.com/evil.tgz",
            "@scope/pkg@../../evil",
        ] {
            assert!(
                ensure_npm_package_spec(refused).is_err(),
                "{refused:?} must be refused"
            );
        }
        for accepted in ["codex-acp", "@google/gemini-cli", "a.b_c-d"] {
            ensure_npm_package_name(accepted)
                .unwrap_or_else(|error| panic!("{accepted:?} should be accepted: {error}"));
        }
        for refused in [
            "",
            "..",
            "Upper",
            "a/b/c",
            "@scope/a/b",
            "@/name",
            "with space",
            "../escape",
        ] {
            assert!(
                ensure_npm_package_name(refused).is_err(),
                "{refused:?} must be refused"
            );
        }
    }

    /// The `zip` a registry entry may be published as is refused with an
    /// explanation rather than half-unpacked: a stock WSL image has no
    /// `unzip`, and unpacking it on Windows would lose the execute bits
    /// (決定 20).
    #[test]
    fn a_zip_is_not_a_wsl_runtime_archive() {
        assert_eq!(WslArchiveKind::Zip.script_mode(), None);
        assert_eq!(WslArchiveKind::TarGz.script_mode(), Some("targz"));
        assert_eq!(WslArchiveKind::TarBz2.script_mode(), Some("tarbz2"));
        assert_eq!(WslArchiveKind::TarXz.script_mode(), Some("tarxz"));
    }

    /// ADR-F49 決定 14 / 25: what comes out of an archive is only ever a
    /// plain file, a directory or a symbolic link that stays inside it, and
    /// nothing keeps a set-user-ID bit. The unpacking runs as the distro's
    /// account outside every namespace, and the result becomes a read grant
    /// for a sandboxed agent, so an archive published without a digest must
    /// not be able to put a device node or a set-user-ID program there.
    #[test]
    fn an_unpacked_archive_holds_nothing_but_files_directories_and_inside_links() {
        assert!(
            ARCHIVE_EXTRACT_SCRIPT
                .contains(r#"special=$(find "$tmp" ! -type d ! -type f ! -type l)"#),
            "the unpacked tree must be scanned for anything but files, directories and links"
        );
        assert!(
            ARCHIVE_EXTRACT_SCRIPT.contains("fail special-entry"),
            "a device node, socket or fifo must fail the whole staging"
        );
        assert!(
            ARCHIVE_EXTRACT_SCRIPT.contains(r#"find "$tmp" -type f -exec chmod u-s,g-s {} +"#),
            "the set-user-ID and set-group-ID bits must be cleared"
        );
        assert!(
            ARCHIVE_EXTRACT_SCRIPT.contains("--no-same-owner"),
            "nothing unpacked may be owned by whoever the archive says"
        );
        // Both checks happen before anything is moved into place.
        let installed = ARCHIVE_EXTRACT_SCRIPT
            .find(r#"mv "$tmp" "$dest""#)
            .expect("the tree is moved into place");
        for check in ["fail special-entry", "fail escaping-link", "chmod u-s,g-s"] {
            let at = ARCHIVE_EXTRACT_SCRIPT
                .find(check)
                .unwrap_or_else(|| panic!("{check} is part of the script"));
            assert!(at < installed, "{check} must run before the tree is used");
        }
    }

    /// ADR-F49 第 2b 段: a Linux command's environment is the one this fork
    /// wrote for the distro, so `PATH` and `HOME` reach it — the very
    /// variables `is_forwardable_env_var` drops from a Windows process's
    /// environment.
    #[test]
    fn a_linux_command_keeps_the_path_and_home_written_for_it() {
        let env = HashMap::from([
            ("PATH".to_string(), "/opt/node/bin:/usr/bin".to_string()),
            ("HOME".to_string(), "/root/staging/home".to_string()),
            ("LANG".to_string(), "ja_JP.UTF-8".to_string()),
            ("=C:".to_string(), "C:\\".to_string()),
            (String::new(), "unnamed".to_string()),
        ]);
        assert_eq!(
            linux_environment(&env),
            vec![
                ("HOME".to_string(), "/root/staging/home".to_string()),
                ("LANG".to_string(), "ja_JP.UTF-8".to_string()),
                ("PATH".to_string(), "/opt/node/bin:/usr/bin".to_string()),
            ],
            "sorted, and without the names setenv(3) itself refuses"
        );
        // The Windows filter is unchanged for the commands that take the
        // project's own environment along.
        let forwarded = forwarded_environment(&env);
        assert!(
            !forwarded
                .iter()
                .any(|(name, _)| name == "PATH" || name == "HOME"),
            "a Windows process's PATH and HOME are still dropped"
        );
    }

    /// ADR-F49 決定 18: a session id reaches the helper as `--session <id>`,
    /// before the `--` the command follows.
    #[test]
    fn a_wsl_command_can_be_registered_as_a_sandbox_session() {
        let argv = crate::build_wsl_sandbox_helper_argv(
            &[],
            &[],
            &[],
            &RemoteSandboxNetwork::Blocked,
            false,
            None,
            &crate::WslHelperOptions {
                sandbox_session_id: Some("abc-123_DEF"),
                ..Default::default()
            },
        );
        let session = argv
            .iter()
            .position(|argument| argument == crate::SANDBOX_SESSION_FLAG)
            .expect("the session flag");
        assert_eq!(
            argv.get(session + 1).map(String::as_str),
            Some("abc-123_DEF")
        );
        let end = argv
            .iter()
            .position(|argument| argument == "--")
            .expect("the end of the policy");
        assert!(session < end, "the session is part of the policy");

        let without = crate::build_wsl_sandbox_helper_argv(
            &[],
            &[],
            &[],
            &RemoteSandboxNetwork::Blocked,
            false,
            None,
            &crate::WslHelperOptions::default(),
        );
        assert!(
            !without
                .iter()
                .any(|argument| argument == crate::SANDBOX_SESSION_FLAG),
            "a command with no id is registered nowhere"
        );
    }

    /// ADR-F49 第 2b 段: the paths of a command that runs *inside* the distro
    /// are Linux-absolute, and every place the policy takes one — a write
    /// grant, a read grant, a protected path and the cwd — accepts that shape
    /// on Windows. (Stage 1 already relied on it for the agent's
    /// configuration folder; the cwd did not accept one until 第 2b 段, which
    /// is what an npm install inside its staging directory needs.)
    #[test]
    fn a_linux_path_is_a_grant_a_windows_client_can_make() {
        let linux = Path::new("/root/.local/share/zed-jp-enhance/node/wsl-sandbox");
        let location =
            crate::HostFilesystemLocation::capture(linux).expect("a Linux path is a grant");
        assert_eq!(location.windows_requested_path(), linux);
        assert_eq!(location.windows_canonical_path(), linux);

        let expected = PathMapping::Wsl(WslPath {
            distro: None,
            path: "/root/.local/share/zed-jp-enhance/node/wsl-sandbox".to_string(),
        });
        assert_eq!(
            path_to_wsl(linux).expect("a writable or readable path"),
            expected
        );
        assert_eq!(
            path_to_wsl_allowing_missing(linux).expect("a protected path"),
            expected
        );
        assert_eq!(directory_to_wsl(linux).expect("a cwd"), expected);
        assert_eq!(
            select_distro(Some(&expected), [&expected]).expect("no distro"),
            None
        );

        // A path that is none of the three grant shapes is still refused
        // everywhere.
        for refused in [
            Path::new("relative/path"),
            Path::new(r"\\server\share\project"),
        ] {
            assert!(crate::HostFilesystemLocation::capture(refused).is_err());
            assert!(directory_to_wsl(refused).is_err());
        }
    }

    /// ADR-F49 第 2b 段: a worktree on the distribution's own ext4 has no
    /// spelling on Windows other than its share, so every grant shape has to
    /// take one — and take it without touching the filesystem, since Windows
    /// cannot follow the distro's symlinks and statting the share needs the
    /// distro running.
    #[test]
    fn a_wsl_share_path_is_a_grant_a_windows_client_can_make() {
        let share = Path::new(r"\\wsl.localhost\Ubuntu\root\zed-x2-e2e");
        // One spelling is stored and compared; the request keeps its own, which
        // is what the UI shows.
        let stored = Path::new(r"\\wsl.localhost\ubuntu\root\zed-x2-e2e");
        let location =
            crate::HostFilesystemLocation::capture(share).expect("a WSL share path is a grant");
        assert_eq!(location.windows_requested_path(), share);
        assert_eq!(location.windows_canonical_path(), stored);

        let expected = PathMapping::Wsl(WslPath {
            distro: Some("Ubuntu".to_string()),
            path: "/root/zed-x2-e2e".to_string(),
        });
        assert_eq!(
            path_to_wsl(share).expect("a writable or readable path"),
            expected
        );
        assert_eq!(
            path_to_wsl_allowing_missing(share).expect("a protected path"),
            expected
        );
        assert_eq!(directory_to_wsl(share).expect("a cwd"), expected);
        assert_eq!(
            select_distro(Some(&expected), [&expected]).expect("one distro"),
            Some("Ubuntu".to_string())
        );

        // The older `\\wsl$\` spelling and the verbatim prefix name the same
        // folder in the same distribution.
        for alias in [
            Path::new(r"\\wsl$\Ubuntu\root\zed-x2-e2e"),
            Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\root\zed-x2-e2e"),
            // WSL matches the distribution name case-insensitively, so this is
            // the same folder too.
            Path::new(r"\\wsl.localhost\UBUNTU\root\zed-x2-e2e"),
        ] {
            assert_eq!(
                crate::HostFilesystemLocation::capture(alias)
                    .expect("an alias is a grant")
                    .windows_canonical_path(),
                stored,
                "{} must be stored in one spelling",
                alias.display()
            );
        }
        for alias in [
            Path::new(r"\\wsl$\Ubuntu\root\zed-x2-e2e"),
            Path::new(r"\\?\UNC\wsl.localhost\Ubuntu\root\zed-x2-e2e"),
        ] {
            assert_eq!(path_to_wsl(alias).expect("an alias of the share"), expected);
        }
    }

    /// One distribution named two ways is one distribution: WSL matches the
    /// name case-insensitively, and so does `wsl_distro_for_roots`, which picks
    /// the session's distro from the very same paths.
    #[test]
    fn select_distro_reads_two_spellings_of_one_distro_as_one() {
        let cwd = PathMapping::Wsl(WslPath {
            distro: Some("Ubuntu".to_string()),
            path: "/root/p".to_string(),
        });
        let grant = PathMapping::Wsl(WslPath {
            distro: Some("ubuntu".to_string()),
            path: "/root/p/sub".to_string(),
        });
        assert_eq!(
            select_distro(Some(&cwd), [&grant]).expect("one distro"),
            Some("Ubuntu".to_string())
        );
    }

    #[test]
    fn wsl_distro_for_path_reads_the_share_a_worktree_is_opened_through() {
        assert_eq!(
            wsl_distro_for_path(Path::new(r"\\wsl.localhost\Ubuntu\root\p")).as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(
            wsl_distro_for_path(Path::new(r"\\wsl$\Ubuntu\root\p")).as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(wsl_distro_for_path(Path::new(r"C:\project")), None);
        assert_eq!(wsl_distro_for_path(Path::new("/root/p")), None);
    }

    /// The `--exec` argument vectors the runtime scripts are run with put the
    /// script's own name first, so `$1` is the first value and nothing is read
    /// as an option.
    #[test]
    fn a_runtime_script_is_given_its_own_name_before_its_arguments() {
        for script in [
            DATA_DIRECTORY_SCRIPT,
            ARCHIVE_CHECK_SCRIPT,
            ARCHIVE_EXTRACT_SCRIPT,
            NPM_STAGING_SCRIPT,
            NPM_FINALIZE_SCRIPT,
            NPM_READ_SCRIPT,
            NPM_EXECUTABLE_SCRIPT,
            CONFIG_DIRECTORY_SCRIPT,
        ] {
            assert!(
                !script.contains("$0"),
                "a runtime script must not depend on its argv[0]"
            );
            assert!(
                script.contains("set -u") || script.starts_with("printf"),
                "a runtime script refuses unset variables"
            );
        }
    }

    /// ADR-F49 決定 25: the completeness check can be asked for one exact
    /// marker or for any, which is what a registry archive published without a
    /// digest — and therefore identified only by where its version and URL put
    /// it — can be asked.
    #[test]
    fn a_staged_runtime_is_found_by_its_marker_or_by_being_there() {
        assert!(
            ARCHIVE_CHECK_SCRIPT.contains(r#"if [ "$3" = any ] || [ "$have" = "$2" ]; then"#),
            "the check script decides between an exact marker and any"
        );
        assert!(
            ARCHIVE_CHECK_SCRIPT.contains(r#"[ -f "$dir/.zed-complete" ]"#),
            "a half-finished staging never counts, whichever is asked for"
        );
    }

    // =======================================================================
    // ADR-F49 第 2b 段: the staging steps, against a `wsl.exe` that is never
    // started
    // =======================================================================

    /// One round trip the code under test asked [`FakeWsl`] to make.
    #[derive(Clone, Debug)]
    struct FakeCall {
        distro: Option<String>,
        args: Vec<String>,
        description: String,
    }

    impl FakeCall {
        /// Which step this is: the name a script is given as its own first
        /// argument (`a_runtime_script_is_given_its_own_name_before_its_arguments`),
        /// or the program for the round trips that run one directly.
        fn step(&self) -> &str {
            match self.args.get(1).map(String::as_str) {
                Some("sh") => self.args.get(4).map(String::as_str).unwrap_or("sh"),
                Some(program) => program,
                None => "",
            }
        }

        /// The script's `$index`.
        fn argument(&self, index: usize) -> &str {
            self.args
                .get(4 + index)
                .map(String::as_str)
                .unwrap_or_default()
        }

        /// The script text this round trip runs.
        fn script(&self) -> &str {
            self.args.get(3).map(String::as_str).unwrap_or_default()
        }

        /// What an `rm` round trip was given after its `--`.
        fn removed(&self) -> Vec<&str> {
            let mut args = self
                .args
                .iter()
                .skip_while(|argument| argument.as_str() != "--");
            args.next();
            args.map(String::as_str).collect()
        }
    }

    /// What the fake answers one step with.
    enum FakeAnswer {
        Printed {
            stdout: String,
            code: i32,
        },
        /// `wsl.exe` started and never came back.
        Wedged,
    }

    /// A Windows file copied into the distro, as the fake was asked to copy
    /// it.
    struct FakeStage {
        directory: String,
        name: String,
        source: PathBuf,
    }

    /// The sandboxed `npm install`, as the fake was asked to run it.
    struct FakeNpmInstall {
        node_directory: String,
        package_spec: String,
        staging: String,
        allowed_hosts: Vec<String>,
        editor_network_allowlist: Option<Vec<String>>,
    }

    /// A `wsl.exe` that is never started: it records what each step asked for
    /// and answers with what the test queued for that step, so the argument
    /// vectors, the order of the round trips and the handling of every answer
    /// can be checked without a distro.
    #[derive(Default)]
    struct FakeWsl {
        calls: Mutex<Vec<FakeCall>>,
        answers: Mutex<HashMap<String, std::collections::VecDeque<FakeAnswer>>>,
        stages: Mutex<Vec<FakeStage>>,
        /// What the copy into the distro reports arriving: the source's own
        /// digest unless a test makes what arrived differ from it.
        staged_digest: Mutex<Option<String>>,
        npm_installs: Mutex<Vec<FakeNpmInstall>>,
        /// What a failing sandboxed `npm install` fails with.
        npm_failure: Mutex<Option<String>>,
        /// Set when a step that never answered was dropped — which is what
        /// `kill_on_drop` turns into a killed `wsl.exe`.
        cancelled: Mutex<bool>,
        memo: WslRuntimeMemo,
    }

    /// The `wsl.exe` every fake is reached through.
    const FAKE_WSL_EXE: &str = r"C:\Windows\System32\wsl.exe";
    /// The Zed data directory a fake distro reports.
    const FAKE_DATA_DIRECTORY: &str = "/home/me/.local/share/zed-jp-enhance";

    /// Marks the fake as cancelled when a step that never answered is
    /// dropped.
    struct WedgedCall<'a>(&'a FakeWsl);

    impl Drop for WedgedCall<'_> {
        fn drop(&mut self) {
            *self
                .0
                .cancelled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        }
    }

    impl FakeWsl {
        fn queue(&self, step: &str, answer: FakeAnswer) -> &Self {
            self.answers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(step.to_string())
                .or_default()
                .push_back(answer);
            self
        }

        /// The next time `step` is reached it prints `stdout` and succeeds.
        fn prints(&self, step: &str, stdout: impl Into<String>) -> &Self {
            self.queue(
                step,
                FakeAnswer::Printed {
                    stdout: stdout.into(),
                    code: 0,
                },
            )
        }

        /// The next time `step` is reached it prints `stdout` and fails.
        fn fails(&self, step: &str, stdout: impl Into<String>) -> &Self {
            self.queue(
                step,
                FakeAnswer::Printed {
                    stdout: stdout.into(),
                    code: 1,
                },
            )
        }

        fn wedges(&self, step: &str) -> &Self {
            self.queue(step, FakeAnswer::Wedged)
        }

        /// [`DATA_DIRECTORY_SCRIPT`]'s answer.
        fn has_data_directory(&self) -> &Self {
            self.prints(
                "zed-wsl-data",
                format!("zed-wsl-data: {FAKE_DATA_DIRECTORY}\n"),
            )
        }

        /// [`ARCHIVE_CHECK_SCRIPT`]'s answer when nothing complete is there.
        fn has_no_runtime(&self) -> &Self {
            self.prints(
                "zed-wsl-archive-check",
                "zed-wsl-archive: missing /nothing\n",
            )
        }

        /// [`ARCHIVE_CHECK_SCRIPT`]'s answer when `path` is there and
        /// complete.
        fn has_runtime(&self, path: &str) -> &Self {
            self.prints(
                "zed-wsl-archive-check",
                format!("zed-wsl-archive: ready {path}\n"),
            )
        }

        /// [`NPM_STAGING_SCRIPT`]'s answer.
        fn makes_npm_staging(&self, path: &str) -> &Self {
            self.prints(
                "zed-wsl-npm-staging",
                format!("zed-wsl-npm: ready {path}\n"),
            )
        }

        /// What arrived in the distro is not what was sent.
        fn corrupts_the_copy(&self, digest: &str) -> &Self {
            *self
                .staged_digest
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(digest.to_string());
            self
        }

        fn npm_install_fails(&self, message: &str) -> &Self {
            *self
                .npm_failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message.to_string());
            self
        }

        fn next_answer(&self, step: &str) -> FakeAnswer {
            let queued = self
                .answers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get_mut(step)
                .and_then(std::collections::VecDeque::pop_front);
            match queued {
                Some(answer) => answer,
                // Removing what a failed step left behind is best effort, and
                // the tests that care look at the call rather than at what it
                // answered.
                None if step == "rm" => FakeAnswer::Printed {
                    stdout: String::new(),
                    code: 0,
                },
                None => panic!("no answer was queued for the step {step:?}"),
            }
        }

        fn calls(&self) -> Vec<FakeCall> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// The steps that were reached, in order.
        fn steps(&self) -> Vec<String> {
            self.calls()
                .iter()
                .map(|call| call.step().to_string())
                .collect()
        }

        /// The one call of `step`, which must have happened exactly once.
        fn only_call(&self, step: &str) -> FakeCall {
            let calls = self.calls();
            let mut matching = calls.iter().filter(|call| call.step() == step);
            let call = matching
                .next()
                .unwrap_or_else(|| panic!("the step {step:?} was never reached"))
                .clone();
            assert!(
                matching.next().is_none(),
                "the step {step:?} was reached more than once"
            );
            call
        }

        fn was_cancelled(&self) -> bool {
            *self
                .cancelled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    impl WslInvoker for FakeWsl {
        fn wsl_exe(&self) -> Result<PathBuf> {
            Ok(PathBuf::from(FAKE_WSL_EXE))
        }

        fn ensure_distro_registered(&self, _distro: Option<&str>) -> Result<()> {
            Ok(())
        }

        fn run<'a>(
            &'a self,
            wsl_exe: &'a Path,
            distro: Option<&'a str>,
            args: Vec<std::ffi::OsString>,
            description: &'a str,
        ) -> futures::future::BoxFuture<'a, Result<std::process::Output>> {
            Box::pin(async move {
                use std::os::windows::process::ExitStatusExt as _;

                assert_eq!(
                    wsl_exe,
                    Path::new(FAKE_WSL_EXE),
                    "every step goes through the installed wsl.exe"
                );
                let call = FakeCall {
                    distro: distro.map(str::to_string),
                    args: args
                        .iter()
                        .map(|argument| argument.to_string_lossy().into_owned())
                        .collect(),
                    description: description.to_string(),
                };
                let step = call.step().to_string();
                self.calls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(call);
                match self.next_answer(&step) {
                    FakeAnswer::Printed { stdout, code } => Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(code as u32),
                        stdout: stdout.into_bytes(),
                        stderr: Vec::new(),
                    }),
                    FakeAnswer::Wedged => {
                        let _wedged = WedgedCall(self);
                        futures::future::pending().await
                    }
                }
            })
        }

        fn stage_file<'a>(
            &'a self,
            wsl_exe: &'a Path,
            _distro: Option<&'a str>,
            directory: &'a str,
            name: &'a str,
            source: &'a Path,
            kind: StagedKind,
        ) -> futures::future::BoxFuture<'a, Result<StagedFile>> {
            Box::pin(async move {
                assert_eq!(wsl_exe, Path::new(FAKE_WSL_EXE));
                assert!(
                    matches!(kind, StagedKind::RuntimeArchive),
                    "the runtime path only ever copies a runtime"
                );
                let metadata = smol::fs::metadata(source)
                    .await
                    .with_context(|| format!("the fake copy reading {}", source.display()))?;
                let corrupted = self
                    .staged_digest
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                let sha256 = match corrupted {
                    Some(digest) => digest,
                    None => file_sha256(source, kind).await?,
                };
                self.stages
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(FakeStage {
                        directory: directory.to_string(),
                        name: name.to_string(),
                        source: source.to_path_buf(),
                    });
                Ok(StagedFile {
                    name: name.to_string(),
                    path: format!("{directory}/.{name}.staged"),
                    size: metadata.len(),
                    sha256,
                })
            })
        }

        fn install_npm_in_sandbox<'a>(
            &'a self,
            node_directory: &'a str,
            package_spec: &'a str,
            staging: &'a str,
            allowed_hosts: Vec<String>,
            editor_network_allowlist: Option<Vec<String>>,
        ) -> futures::future::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.npm_installs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(FakeNpmInstall {
                        node_directory: node_directory.to_string(),
                        package_spec: package_spec.to_string(),
                        staging: staging.to_string(),
                        allowed_hosts,
                        editor_network_allowlist,
                    });
                match self
                    .npm_failure
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                {
                    Some(message) => Err(anyhow::anyhow!(message)),
                    None => Ok(()),
                }
            })
        }

        fn memo(&self) -> &WslRuntimeMemo {
            &self.memo
        }
    }

    /// A file on this machine with `contents` in it, and its SHA-256 — what a
    /// download hands the staging steps.
    fn downloaded(contents: &[u8]) -> (tempfile::TempDir, PathBuf, String) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("runtime.tar.gz");
        std::fs::write(&path, contents).expect("writing the downloaded file");
        let digest = hex_string(&Sha256::digest(contents));
        (directory, path, digest)
    }

    const NPX_RELATIVE: &str = "external_agents/wsl-sandbox/registry/npx/claude";
    const NODE_RELATIVE: &str = "node/wsl-sandbox/node-v24.11.0-linux-x64";
    const PACKAGE_SPEC: &str = "@agentclientprotocol/claude-agent-acp@0.76.0";

    /// ADR-F49 決定 22: an npm package is installed by putting a staging
    /// directory in the distro, running npm **inside the sandbox** against it,
    /// and moving what it produced into the Zed data directory the helper
    /// protects — in that order, and only after the check found nothing
    /// complete already there.
    #[test]
    fn an_npm_package_is_installed_through_a_staging_directory_the_sandbox_may_write() {
        let fake = FakeWsl::default();
        let destination = format!("{FAKE_DATA_DIRECTORY}/{NPX_RELATIVE}");
        fake.has_data_directory()
            .has_no_runtime()
            .makes_npm_staging("/home/me/.local/share/zed-jp-enhance-staging/abc")
            .prints(
                "zed-wsl-npm-finalize",
                format!("zed-wsl-npm: ready {destination}\n"),
            )
            .has_runtime(&destination);

        let installed = smol::block_on(install_npm_package(
            &fake,
            Some("Ubuntu"),
            "/home/me/.local/share/zed-jp-enhance/node/wsl-sandbox/node-v24.11.0-linux-x64",
            PACKAGE_SPEC,
            NPX_RELATIVE,
            vec!["registry.npmjs.org".to_string()],
            Some(vec!["nodejs.org".to_string()]),
        ))
        .expect("the install");
        assert_eq!(installed, destination);

        assert_eq!(
            fake.steps(),
            vec![
                "zed-wsl-data",
                "zed-wsl-archive-check",
                "zed-wsl-npm-staging",
                "zed-wsl-npm-finalize",
                "zed-wsl-archive-check",
            ]
        );

        let data = fake.only_call("zed-wsl-data");
        assert_eq!(data.distro.as_deref(), Some("Ubuntu"));
        assert_eq!(data.description, "checking Zed's data folder");
        assert_eq!(data.argument(1), LINUX_DEFAULT_DATA_HOME_RELATIVE);
        assert_eq!(data.argument(2), LINUX_APP_DIRECTORY_NAME);

        let checks: Vec<FakeCall> = fake
            .calls()
            .into_iter()
            .filter(|call| call.step() == "zed-wsl-archive-check")
            .collect();
        for check in &checks {
            assert_eq!(check.argument(1), destination);
            assert_eq!(check.argument(2), PACKAGE_SPEC, "the marker is the spec");
            assert_eq!(check.argument(3), "exact");
        }

        // The staging directory is beside the Zed data directory, never in it:
        // it is the one place the sandboxed npm may write.
        let staging = fake.only_call("zed-wsl-npm-staging");
        assert_eq!(staging.argument(1), WSL_STAGING_HOME_RELATIVE);
        let suffix = staging.argument(2).to_string();
        assert!(!suffix.is_empty(), "the staging directory has its own name");

        let installs = fake
            .npm_installs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let install = installs.first().expect("npm ran once");
        assert_eq!(installs.len(), 1);
        assert_eq!(install.package_spec, PACKAGE_SPEC);
        assert_eq!(
            install.staging,
            "/home/me/.local/share/zed-jp-enhance-staging/abc"
        );
        assert_eq!(
            install.node_directory,
            "/home/me/.local/share/zed-jp-enhance/node/wsl-sandbox/node-v24.11.0-linux-x64"
        );
        assert_eq!(
            install.allowed_hosts,
            vec!["registry.npmjs.org".to_string()]
        );
        assert_eq!(
            install.editor_network_allowlist,
            Some(vec!["nodejs.org".to_string()])
        );

        let finalize = fake.only_call("zed-wsl-npm-finalize");
        assert_eq!(
            finalize.argument(1),
            "/home/me/.local/share/zed-jp-enhance-staging/abc"
        );
        assert_eq!(finalize.argument(2), destination);
        assert_eq!(finalize.argument(3), suffix, "one install, one suffix");
        assert_eq!(
            finalize.argument(4),
            PACKAGE_SPEC,
            "the marker records what was asked for"
        );
    }

    /// An install already there for the same specification costs one round
    /// trip and never runs npm again.
    #[test]
    fn an_installed_npm_package_is_reused_untouched() {
        let fake = FakeWsl::default();
        let destination = format!("{FAKE_DATA_DIRECTORY}/{NPX_RELATIVE}");
        fake.has_data_directory().has_runtime(&destination);

        let installed = smol::block_on(install_npm_package(
            &fake,
            None,
            "/node",
            PACKAGE_SPEC,
            NPX_RELATIVE,
            Vec::new(),
            None,
        ))
        .expect("the installed package");
        assert_eq!(installed, destination);
        assert_eq!(fake.steps(), vec!["zed-wsl-data", "zed-wsl-archive-check"]);
        assert!(
            fake.npm_installs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "nothing is installed again"
        );
    }

    /// ADR-F49 決定 22: npm is only ever handed a package from the registry.
    /// A `file:` / `github:` / `npm:` specification — every one of which npm
    /// would take from somewhere else, a directory the sandbox itself may
    /// write among them — is refused before `wsl.exe` is started at all.
    #[test]
    fn a_specification_npm_would_take_from_elsewhere_never_reaches_wsl() {
        for refused in [
            "claude-agent-acp@file:../staging/pkg",
            "claude-agent-acp@file:/tmp/evil",
            "github:owner/repo",
            "claude-agent-acp@npm:other-package",
            "claude-agent-acp@git+https://example.com/evil.git",
            "https://example.com/evil.tgz",
            "--global",
        ] {
            let fake = FakeWsl::default();
            let error = smol::block_on(install_npm_package(
                &fake,
                None,
                "/node",
                refused,
                NPX_RELATIVE,
                Vec::new(),
                None,
            ))
            .expect_err(&format!("{refused:?} must be refused"));
            assert!(
                format!("{error:#}").contains("invalid npm package specification"),
                "{refused:?} was refused with {error:#}"
            );
            assert!(
                fake.calls().is_empty(),
                "{refused:?} must not reach wsl.exe"
            );
        }

        // The same before the install destination is used to build a path.
        let fake = FakeWsl::default();
        smol::block_on(install_npm_package(
            &fake,
            None,
            "/node",
            PACKAGE_SPEC,
            "../../.ssh",
            Vec::new(),
            None,
        ))
        .expect_err("a destination outside the data directory must be refused");
        assert!(fake.calls().is_empty());
    }

    /// A failed `npm install` takes its staging directory with it and the
    /// install is never moved into place; the error the user sees is npm's.
    #[test]
    fn a_failed_npm_install_leaves_no_staging_directory_behind() {
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .has_no_runtime()
            .makes_npm_staging("/home/me/.local/share/zed-jp-enhance-staging/abc")
            .npm_install_fails("Could not run `npm install x` in the sandbox");

        let error = smol::block_on(install_npm_package(
            &fake,
            None,
            "/node",
            PACKAGE_SPEC,
            NPX_RELATIVE,
            Vec::new(),
            None,
        ))
        .expect_err("the failed install");
        assert!(
            format!("{error:#}").contains("Could not run `npm install x` in the sandbox"),
            "the user is shown what npm failed with: {error:#}"
        );
        assert!(
            !fake
                .steps()
                .iter()
                .any(|step| step == "zed-wsl-npm-finalize"),
            "nothing half-installed is moved into place"
        );
        assert_eq!(
            fake.only_call("rm").removed(),
            vec!["/home/me/.local/share/zed-jp-enhance-staging/abc"]
        );
    }

    /// The trusted side moving the install into place is what can still fail
    /// after npm succeeded; the one-word reason the script reports reaches the
    /// user, and the staging directory still goes.
    #[test]
    fn an_install_that_produced_nothing_is_not_moved_into_place() {
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .has_no_runtime()
            .makes_npm_staging("/home/me/.local/share/zed-jp-enhance-staging/abc")
            .fails(
                "zed-wsl-npm-finalize",
                "zed-wsl-npm: error no-node-modules\n",
            );

        let error = smol::block_on(install_npm_package(
            &fake,
            Some("Ubuntu"),
            "/node",
            PACKAGE_SPEC,
            NPX_RELATIVE,
            Vec::new(),
            None,
        ))
        .expect_err("the failed finalize");
        let message = format!("{error:#}");
        assert!(
            message.contains("Could not install the npm package")
                && message.contains("(no-node-modules)")
                && message.contains("Ubuntu"),
            "{message}"
        );
        assert_eq!(
            fake.only_call("rm").removed(),
            vec!["/home/me/.local/share/zed-jp-enhance-staging/abc"]
        );
    }

    /// ADR-F49 決定 22: the trusted side reads the package's own manifest
    /// following no link at any step, and nothing of the package is run to get
    /// at it.
    #[test]
    fn a_package_manifest_is_read_one_plain_component_at_a_time() {
        let install_directory = format!("{FAKE_DATA_DIRECTORY}/{NPX_RELATIVE}");
        let fake = FakeWsl::default();
        fake.prints(
            "zed-wsl-npm-read",
            format!(
                "zed-wsl-npm: file {install_directory}/node_modules/@agentclientprotocol/\
                 claude-agent-acp/package.json\n{{\"version\":\"0.76.0\"}}\n"
            ),
        );
        let package_json = smol::block_on(read_npm_package_json(
            &fake,
            None,
            &install_directory,
            "@agentclientprotocol/claude-agent-acp",
        ))
        .expect("the manifest")
        .expect("something installed");
        assert_eq!(package_json, "{\"version\":\"0.76.0\"}\n");

        let read = fake.only_call("zed-wsl-npm-read");
        assert_eq!(read.script(), NPM_READ_SCRIPT);
        assert_eq!(
            &read.args[4..],
            [
                "zed-wsl-npm-read",
                install_directory.as_str(),
                "node_modules",
                "@agentclientprotocol",
                "claude-agent-acp",
                "package.json",
            ]
        );

        // Nothing installed there is not an error: it is installed again.
        let fake = FakeWsl::default();
        fake.prints(
            "zed-wsl-npm-read",
            format!("zed-wsl-npm: missing {install_directory}\n"),
        );
        assert_eq!(
            smol::block_on(read_npm_package_json(
                &fake,
                None,
                &install_directory,
                "claude-agent-acp"
            ))
            .expect("a missing install"),
            None
        );

        // A link on the way to it is.
        let fake = FakeWsl::default();
        fake.fails(
            "zed-wsl-npm-read",
            "zed-wsl-npm: error link /home/me/.local/share/zed-jp-enhance/x\n",
        );
        let error = smol::block_on(read_npm_package_json(
            &fake,
            None,
            &install_directory,
            "claude-agent-acp",
        ))
        .expect_err("a link must be refused");
        assert!(
            format!("{error:#}").contains("(link /home/me/.local/share/zed-jp-enhance/x)"),
            "{error:#}"
        );
    }

    /// The executable a manifest declares is confirmed to be a plain file
    /// inside the install before it is ever started.
    #[test]
    fn a_package_executable_is_confirmed_before_it_is_started() {
        let install_directory = format!("{FAKE_DATA_DIRECTORY}/{NPX_RELATIVE}");
        let executable = format!("{install_directory}/node_modules/claude-agent-acp/dist/index.js");
        let fake = FakeWsl::default();
        fake.prints(
            "zed-wsl-npm-executable",
            format!("zed-wsl-npm: executable {executable}\n"),
        );
        assert_eq!(
            smol::block_on(npm_package_executable(
                &fake,
                None,
                &install_directory,
                "claude-agent-acp",
                "./dist/index.js",
            ))
            .expect("the executable"),
            executable
        );
        assert_eq!(
            &fake.only_call("zed-wsl-npm-executable").args[4..],
            [
                "zed-wsl-npm-executable",
                install_directory.as_str(),
                "node_modules",
                "claude-agent-acp",
                "dist",
                "index.js",
            ]
        );

        let fake = FakeWsl::default();
        fake.fails(
            "zed-wsl-npm-executable",
            "zed-wsl-npm: error not-a-file /x/dist\n",
        );
        let error = smol::block_on(npm_package_executable(
            &fake,
            None,
            &install_directory,
            "claude-agent-acp",
            "./dist",
        ))
        .expect_err("a directory is no executable");
        assert!(
            format!("{error:#}").contains("(not-a-file /x/dist)"),
            "{error:#}"
        );
    }

    /// ADR-F49 決定 20: an archive is checked on Windows, copied beside where
    /// it will land, and unpacked **inside the distro** against the same
    /// digest — and only then is the result confirmed.
    #[test]
    fn an_archive_is_checked_copied_and_unpacked_inside_the_distro() {
        let (_directory, source, digest) = downloaded(b"a Linux Node.js");
        let destination = format!("{FAKE_DATA_DIRECTORY}/{NODE_RELATIVE}");
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .has_no_runtime()
            .prints(
                "zed-wsl-archive-extract",
                format!("zed-wsl-archive: ready {destination}\n"),
            )
            .has_runtime(&destination);

        let staged = smol::block_on(stage_runtime_archive(
            &fake,
            None,
            NODE_RELATIVE,
            &source,
            &digest,
            WslArchiveKind::TarGz,
            true,
        ))
        .expect("the staged Node.js");
        assert_eq!(staged, destination);
        assert_eq!(
            fake.steps(),
            vec![
                "zed-wsl-data",
                "zed-wsl-archive-check",
                "zed-wsl-archive-extract",
                "zed-wsl-archive-check",
            ]
        );

        // Copied into the destination's own parent, so the unpacked tree is
        // moved into place within one filesystem.
        let stages = fake
            .stages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stage = stages.first().expect("one copy into the distro");
        assert_eq!(stages.len(), 1);
        assert_eq!(
            stage.directory,
            format!("{FAKE_DATA_DIRECTORY}/node/wsl-sandbox")
        );
        assert_eq!(stage.name, "runtime-archive");
        assert_eq!(stage.source, source);

        let extract = fake.only_call("zed-wsl-archive-extract");
        assert_eq!(extract.script(), ARCHIVE_EXTRACT_SCRIPT);
        assert_eq!(extract.argument(1), destination);
        assert_eq!(
            extract.argument(2),
            format!("{FAKE_DATA_DIRECTORY}/node/wsl-sandbox/.runtime-archive.staged")
        );
        assert_eq!(
            extract.argument(3),
            digest,
            "what arrived is hashed again inside the distro"
        );
        assert_eq!(extract.argument(4), "targz");
        assert_eq!(
            extract.argument(5),
            "1",
            "a Node.js tarball wraps everything in one directory"
        );
        assert!(
            !extract.argument(6).is_empty(),
            "a temporary name of its own"
        );
    }

    /// ADR-F49 決定 20: a `zip` is refused with an explanation, before
    /// anything is copied — a stock WSL image has no `unzip`, and unpacking it
    /// on Windows would lose the execute bits.
    #[test]
    fn a_zip_never_reaches_the_distro() {
        let (_directory, source, digest) = downloaded(b"a zip");
        let fake = FakeWsl::default();
        let error = smol::block_on(stage_runtime_archive(
            &fake,
            None,
            NODE_RELATIVE,
            &source,
            &digest,
            WslArchiveKind::Zip,
            false,
        ))
        .expect_err("a zip must be refused");
        assert!(format!("{error:#}").contains("unzip"), "{error:#}");
        assert!(fake.calls().is_empty(), "nothing was started for a zip");
    }

    /// ADR-F49 決定 21: an archive that is not the one this build pins — the
    /// Node.js tarball against its embedded SHA-256 — is refused before a byte
    /// of it is copied into the distro.
    #[test]
    fn an_archive_that_is_not_the_one_pinned_is_never_copied_in() {
        let (_directory, source, digest) = downloaded(b"something else");
        let pinned = "0".repeat(64);
        let fake = FakeWsl::default();
        fake.has_data_directory().has_no_runtime();

        let error = smol::block_on(stage_runtime_archive(
            &fake,
            None,
            NODE_RELATIVE,
            &source,
            &pinned,
            WslArchiveKind::TarGz,
            true,
        ))
        .expect_err("a mismatched archive must be refused");
        let message = format!("{error:#}");
        assert!(
            message.contains(&pinned) && message.contains(&digest),
            "{message}"
        );
        assert_eq!(fake.steps(), vec!["zed-wsl-data", "zed-wsl-archive-check"]);
        assert!(
            fake.stages
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "nothing was copied into the distro"
        );
    }

    /// What arrived in the distro is hashed again on the way in: a copy that
    /// does not match what was sent is discarded and never unpacked.
    #[test]
    fn a_copy_that_does_not_match_what_was_sent_is_discarded() {
        let (_directory, source, digest) = downloaded(b"a Linux Node.js");
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .has_no_runtime()
            .corrupts_the_copy(&"f".repeat(64));

        let error = smol::block_on(stage_runtime_archive(
            &fake,
            None,
            NODE_RELATIVE,
            &source,
            &digest,
            WslArchiveKind::TarGz,
            true,
        ))
        .expect_err("a corrupted copy must be refused");
        assert!(
            format!("{error:#}").contains("changed while it was being copied into WSL"),
            "{error:#}"
        );
        assert!(
            !fake
                .steps()
                .iter()
                .any(|step| step == "zed-wsl-archive-extract"),
            "nothing is unpacked from a copy that does not match"
        );
        assert_eq!(
            fake.only_call("rm").removed(),
            vec![format!(
                "{FAKE_DATA_DIRECTORY}/node/wsl-sandbox/.runtime-archive.staged"
            )]
        );
    }

    /// ADR-F49 決定 14 / 25: every reason the distro refuses an unpacked tree
    /// — a device node or a set-user-ID program among them — reaches the user
    /// by name, and the copy it was unpacked from is removed either way.
    #[test]
    fn an_archive_the_distro_refuses_is_named_and_leaves_nothing_behind() {
        for reason in [
            "special-entry",
            "escaping-link",
            "unsafe-entry",
            "digest-mismatch",
            "cannot-clear-setid",
        ] {
            let (_directory, source, digest) = downloaded(b"a Linux Node.js");
            let fake = FakeWsl::default();
            fake.has_data_directory().has_no_runtime().fails(
                "zed-wsl-archive-extract",
                format!("zed-wsl-archive: error {reason}\n"),
            );

            let error = smol::block_on(stage_runtime_archive(
                &fake,
                Some("Ubuntu"),
                NODE_RELATIVE,
                &source,
                &digest,
                WslArchiveKind::TarGz,
                true,
            ))
            .expect_err("a refused tree must be an error");
            let message = format!("{error:#}");
            assert!(
                message.contains("Could not extract the agent runtime")
                    && message.contains(&format!("({reason})")),
                "{message}"
            );
            assert_eq!(
                fake.only_call("rm").removed(),
                vec![format!(
                    "{FAKE_DATA_DIRECTORY}/node/wsl-sandbox/.runtime-archive.staged"
                )]
            );
        }
    }

    /// ADR-F49 決定 25: a runtime is found under the digest the registry names
    /// where there is one, and simply for being complete where it is where
    /// there is not.
    #[test]
    fn a_staged_runtime_is_asked_for_by_its_marker_or_for_being_there() {
        let destination = format!("{FAKE_DATA_DIRECTORY}/{NODE_RELATIVE}");
        let digest = "a".repeat(64);

        let fake = FakeWsl::default();
        fake.has_data_directory().has_runtime(&destination);
        assert_eq!(
            smol::block_on(staged_runtime(&fake, None, NODE_RELATIVE, Some(&digest)))
                .expect("the staged runtime"),
            Some(destination.clone())
        );
        let check = fake.only_call("zed-wsl-archive-check");
        assert_eq!(check.argument(1), destination);
        assert_eq!(check.argument(2), digest);
        assert_eq!(check.argument(3), "exact");

        let fake = FakeWsl::default();
        fake.has_data_directory().has_runtime(&destination);
        assert_eq!(
            smol::block_on(staged_runtime(&fake, None, NODE_RELATIVE, None))
                .expect("the staged runtime"),
            Some(destination)
        );
        let check = fake.only_call("zed-wsl-archive-check");
        assert_eq!(check.argument(2), "");
        assert_eq!(check.argument(3), "any");

        // A half-finished staging is not there at all, and is staged again.
        let fake = FakeWsl::default();
        fake.has_data_directory().has_no_runtime();
        assert_eq!(
            smol::block_on(staged_runtime(&fake, None, NODE_RELATIVE, Some(&digest)))
                .expect("nothing staged"),
            None
        );
    }

    /// ADR-F49 決定 25: a registry entry published as a plain Linux executable
    /// is put in a runtime directory of its own, made executable, and its
    /// digest recorded as the marker.
    #[test]
    fn a_plain_linux_executable_is_staged_as_a_runtime_of_its_own() {
        let (_directory, source, digest) = downloaded(b"#!/bin/sh\n");
        let relative = "external_agents/wsl-sandbox/registry/archive/some-agent/v1";
        let destination = format!("{FAKE_DATA_DIRECTORY}/{relative}");
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .has_no_runtime()
            .prints(
                "zed-wsl-runtime-file",
                format!("zed-wsl-archive: ready {destination}\n"),
            )
            .has_runtime(&destination);

        let staged = smol::block_on(stage_runtime_file(
            &fake,
            None,
            relative,
            "some-agent",
            &source,
            &digest,
            true,
        ))
        .expect("the staged executable");
        assert_eq!(staged, format!("{destination}/some-agent"));

        let placed = fake.only_call("zed-wsl-runtime-file");
        assert_eq!(placed.script(), FILE_STAGE_SCRIPT);
        assert_eq!(placed.argument(1), destination);
        assert_eq!(placed.argument(3), digest);
        assert_eq!(placed.argument(4), "some-agent");
        assert_eq!(
            placed.argument(5),
            "1",
            "it is the program the agent runs as"
        );
    }

    /// Both answers a distro is asked for once are then remembered: a second
    /// step in the same distro makes no round trip of its own.
    #[test]
    fn the_data_directory_and_architecture_are_asked_of_a_distro_once() {
        let fake = FakeWsl::default();
        fake.has_data_directory()
            .prints("sh", "zed-wsl-arch: x86_64\n");

        for _ in 0..2 {
            assert_eq!(
                smol::block_on(runtime_data_directory(&fake, Some("Ubuntu")))
                    .expect("the data directory"),
                FAKE_DATA_DIRECTORY
            );
            assert_eq!(
                smol::block_on(machine_architecture(&fake, Some("Ubuntu")))
                    .expect("the architecture"),
                "x86_64"
            );
        }
        assert_eq!(fake.steps(), vec!["zed-wsl-data", "sh"]);

        // Another distro is another answer.
        fake.has_data_directory();
        smol::block_on(runtime_data_directory(&fake, Some("Debian")))
            .expect("the other distro's data directory");
        assert_eq!(fake.steps().len(), 3);
    }

    /// The `sandbox` crate deliberately keeps no timer: a wedged `wsl.exe` is
    /// bounded by the caller (`project`'s `wsl_runtime::bounded`), whose
    /// timeout drops this future — and dropping it is what kills the
    /// `wsl.exe` it left running (`kill_on_drop`).
    #[test]
    fn a_wedged_wsl_is_left_killed_when_the_step_is_dropped() {
        use futures::FutureExt as _;

        let fake = FakeWsl::default();
        fake.has_data_directory().wedges("zed-wsl-archive-check");

        let step = staged_runtime(&fake, None, NODE_RELATIVE, None);
        assert!(
            step.now_or_never().is_none(),
            "a wedged wsl.exe never answers"
        );
        assert!(
            fake.was_cancelled(),
            "dropping the step must take the wsl.exe with it"
        );
    }

    /// These messages are put together from pieces (the distro, the file,
    /// the step, the exit status), which have to read as one sentence in each
    /// language.
    #[test]
    fn composed_unavailable_messages_read_as_sentences() {
        use std::os::windows::process::ExitStatusExt as _;

        let failed = |stdout: &str, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let messages = || {
            [
                unverifiable_helper(Some("Ubuntu")).to_string(),
                copy_failure(
                    Some("Ubuntu"),
                    &failed("", "disk full"),
                    StagedKind::RuntimeArchive,
                )
                .to_string(),
                runtime_script_failure(
                    None,
                    &failed("zed-wsl-npm: error no-node-modules\n", ""),
                    NPM_RESULT_PREFIX,
                    i18n::t!("Could not install the npm package"),
                )
                .to_string(),
                wsl_start_failure(
                    i18n::tc!("wsl sandbox step", "resolving paths"),
                    &std::io::Error::other("boom"),
                )
                .to_string(),
            ]
        };

        assert_eq!(
            messages(),
            [
                "Could not create the sandbox in WSL: `sha256sum` is missing in the WSL distro \
                 `Ubuntu`, so it cannot be verified that the sandbox helper has not been replaced. \
                 Install coreutils and try again.",
                "Could not create the sandbox in WSL: Could not copy the agent runtime archive to \
                 the WSL distro `Ubuntu` (exit code 1, error output: disk full). Check the free \
                 space in WSL and that the home directory is writable.",
                "Could not create the sandbox in WSL: Could not install the npm package in the \
                 default WSL distro (no-node-modules) (exit code 1).",
                "Could not create the sandbox in WSL: Could not start WSL for resolving paths \
                 (boom). Run `wsl --update` or restart Windows, then try again.",
            ]
        );

        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(
            messages(),
            [
                "WSL でサンドボックスを作成できませんでした: WSL ディストリビューション `Ubuntu`に \
                 `sha256sum` がないため、サンドボックス用のヘルパーが置き換えられていないことを\
                 確認できません。coreutils をインストールしてから再試行してください。",
                "WSL でサンドボックスを作成できませんでした: エージェントの実行環境のアーカイブを\
                 WSL ディストリビューション `Ubuntu`にコピーできませんでした (終了コード 1、\
                 エラー出力: disk full)。WSL 内の空き容量と、ホームディレクトリへの書き込み権限を\
                 確認してください。",
                "WSL でサンドボックスを作成できませんでした: 既定の WSL ディストリビューションで\
                 npm パッケージを配置できませんでした (no-node-modules) (終了コード 1)。",
                "WSL でサンドボックスを作成できませんでした: パスの解決のために WSL を起動できません\
                 でした (boom)。`wsl --update` を実行するか、Windows を再起動してから再試行してください。",
            ]
        );
    }

    /// What a failed `npm install` printed reaches the user, with its exit
    /// code and only the tail of a long log.
    #[test]
    fn a_failed_npm_install_is_reported_with_its_own_output() {
        use std::os::windows::process::ExitStatusExt as _;

        let failed = |stdout: &str, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let details = npm_failure_details(&failed("", "npm ERR! 404 Not Found"));
        assert_eq!(
            details,
            " (exit code 1, npm output: npm ERR! 404 Not Found)"
        );

        // Nothing on standard error falls back to what npm printed.
        assert!(
            npm_failure_details(&failed("npm ERR! code E404", "")).contains("npm ERR! code E404")
        );

        let long = "あ".repeat(1000);
        let details = npm_failure_details(&failed("", &long));
        assert!(
            details
                .chars()
                .filter(|character| *character == 'あ')
                .count()
                == 800,
            "only the tail of a long log is shown"
        );
    }

    /// Two staging steps of this process never share a temporary name.
    #[test]
    fn a_staging_suffix_is_used_once() {
        let first = unique_suffix();
        let second = unique_suffix();
        assert_ne!(first, second);
        for suffix in [first, second] {
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
                "{suffix} can never be anything but one path component"
            );
        }
    }

    // =======================================================================
    // ADR-F49 第 2b 段: end-to-end, against this machine's real WSL
    // =======================================================================
    //
    // Ignored by default and gated on `ZED_E2E_WSL_RUNTIME=1`, because they
    // stage a real Node.js into the default distro, install a real npm
    // package inside the sandbox and leave processes running while they look
    // at them. Run them one at a time, in order:
    //
    //     set ZED_E2E_WSL_RUNTIME=1
    //     set ZED_E2E_NODE_TARBALL=<node-v24.11.0-linux-x64.tar.gz>
    //     set ZED_E2E_NODE_SHA256=<its digest>
    //     rem 既定は zed-e2e。名前に e2e を含む、本番と重ならない根にだけ置ける。
    //     set ZED_E2E_WSL_ROOT=<staging root below the data directory>
    //     cargo test -p sandbox --target-dir ... e2e_ -- --ignored --nocapture --test-threads=1
    //
    // `e2e_zz_clean_up` removes everything they staged.

    /// Everything the end-to-end tests stage, so they can be told apart from
    /// what a real Zed put in the distro and removed again afterwards.
    #[cfg(target_os = "windows")]
    #[allow(
        clippy::disallowed_methods,
        clippy::zombie_processes,
        reason = "an end-to-end driver that deliberately blocks on wsl.exe and \
                  leaves one command running while it looks at the sandbox"
    )]
    mod e2e {
        pub(super) const PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";
        pub(super) const PACKAGE_SPEC: &str = "@agentclientprotocol/claude-agent-acp@0.76.0";
        pub(super) const PROJECT_KEY: &str = "00112233445566ff";

        /// データディレクトリからの相対で、E2E が置くものすべての根。本番と同じ場所に
        /// 置くと `e2e_zz_clean_up` の `rm -rf` がユーザーの staged Node.js を消すので、
        /// 本番の木とは重ねない。同じディストリビューションで複数の run を区別したい
        /// ときのために `ZED_E2E_WSL_ROOT` で差し替えられる。
        pub(super) const DEFAULT_ROOT: &str = "zed-e2e";

        /// 本物の Zed がディストリビューションのデータディレクトリに置くものの、先頭の
        /// 成分 (`project` の `agent_server_store` が持つ綴り。この crate からは参照
        /// できないので書き出してある)。E2E の根はこのどちらでもない。
        pub(super) const PRODUCTION_ROOTS: [&str; 2] = ["node", "external_agents"];

        pub(super) fn enabled() -> bool {
            std::env::var("ZED_E2E_WSL_RUNTIME").is_ok_and(|value| value == "1")
        }

        /// `root` が E2E 専用の場所か。staging が受け付ける相対パスで、本番が使う木の外で、
        /// かつ根の名前が E2E のものと名乗っていること。データフォルダーには
        /// `PRODUCTION_ROOTS` 以外にも本番のものが増えるので、名乗った根だけを消す。
        pub(super) fn is_isolated_root(root: &str) -> bool {
            super::ensure_safe_relative_path(root, "end-to-end root").is_ok()
                && root.split('/').next().is_some_and(|first| {
                    !PRODUCTION_ROOTS.contains(&first) && first.contains("e2e")
                })
        }

        pub(super) fn root_relative() -> String {
            let root =
                std::env::var("ZED_E2E_WSL_ROOT").unwrap_or_else(|_| DEFAULT_ROOT.to_owned());
            assert!(
                is_isolated_root(&root),
                "ZED_E2E_WSL_ROOT={root:?} だと本番が置いたものを消してしまう"
            );
            root
        }

        /// 根の下の綴りは本番と同じにしてあるので、スクリプトから見える入れ子は本物と
        /// 同じになる。
        pub(super) fn staged_relative(relative: &str) -> String {
            format!("{}/{relative}", root_relative())
        }

        pub(super) fn node_relative() -> String {
            staged_relative("node/wsl-sandbox/node-v24.11.0-linux-x64")
        }

        pub(super) fn install_relative() -> String {
            staged_relative("external_agents/wsl-sandbox/registry/npx/zed-e2e")
        }

        /// The staged Linux Node.js, staging it from `ZED_E2E_NODE_TARBALL`
        /// first if it is not there yet.
        pub(super) fn node_directory() -> String {
            let digest = std::env::var("ZED_E2E_NODE_SHA256").expect("ZED_E2E_NODE_SHA256");
            let relative = node_relative();
            if let Some(staged) =
                smol::block_on(super::staged_wsl_runtime(None, &relative, Some(&digest)))
                    .expect("checking for a staged Node.js")
            {
                return staged;
            }
            let tarball = std::env::var("ZED_E2E_NODE_TARBALL").expect("ZED_E2E_NODE_TARBALL");
            smol::block_on(super::stage_wsl_runtime_archive(
                None,
                &relative,
                std::path::Path::new(&tarball),
                &digest,
                super::WslArchiveKind::TarGz,
                true,
            ))
            .expect("staging the Linux Node.js")
        }

        /// Runs `program` in a real WSL sandbox that may write `writable` and
        /// read `readable`, with `stdin` on its standard input, and returns
        /// what it printed.
        pub(super) fn run_in_sandbox(
            program: &str,
            args: &[&str],
            writable: &[&str],
            readable: &[&str],
            environment: &[(&str, &str)],
            session_id: Option<&str>,
            stdin: Option<&[u8]>,
            wait: bool,
        ) -> (std::process::Output, crate::Sandbox) {
            use std::io::Write as _;

            let mut sandbox = crate::Sandbox::new(crate::SandboxPolicy {
                fs: crate::SandboxFsPolicy::Restricted {
                    writable_paths: writable
                        .iter()
                        .map(|path| {
                            crate::HostFilesystemLocation::capture(path).expect("a write grant")
                        })
                        .collect(),
                    protected_paths: Vec::new(),
                    readable_paths: readable
                        .iter()
                        .map(|path| {
                            crate::HostFilesystemLocation::capture(path).expect("a read grant")
                        })
                        .collect(),
                },
                network: crate::SandboxNetPolicy::Blocked,
            })
            .expect("a sandbox");
            sandbox.set_wsl_linux_environment();
            if let Some(session_id) = session_id {
                sandbox.set_sandbox_session_id(session_id.to_string());
            }
            let wrapped = smol::block_on(
                sandbox.wrap(&crate::CommandAndArgs {
                    program: program.to_string(),
                    args: args.iter().map(|argument| argument.to_string()).collect(),
                    env: environment
                        .iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect(),
                    cwd: None,
                }),
            )
            .expect("wrapping the command");

            let mut command = std::process::Command::new(&wrapped.program);
            command
                .args(&wrapped.args)
                .envs(&wrapped.env)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(if stdin.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                });
            let mut child = command.spawn().expect("starting wsl.exe");
            if let Some(stdin) = stdin {
                child
                    .stdin
                    .as_mut()
                    .expect("a stdin pipe")
                    .write_all(stdin)
                    .expect("writing to the command");
            }
            if !wait {
                // The caller looks at the sandbox while the command runs; it
                // reads the output itself.
                return (
                    std::process::Output {
                        status: Default::default(),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    },
                    sandbox,
                );
            }
            let output = child.wait_with_output().expect("waiting for the command");
            (output, sandbox)
        }

        /// The executable the package declares, as
        /// `node_runtime::package_json_executable` reads it — spelled out
        /// here because this crate deliberately has no JSON parser of its own
        /// (the real caller is the `project` crate, which does).
        pub(super) fn bin_from_package_json(package_json: &str) -> String {
            let rest = package_json
                .split("\"bin\"")
                .nth(1)
                .expect("a bin in the package.json");
            let rest = rest
                .trim_start()
                .strip_prefix(':')
                .expect("a value after bin")
                .trim_start();
            let rest = match rest.strip_prefix('{') {
                // `{"name": "path"}`: the value of the first entry.
                Some(object) => object.split(':').nth(1).expect("a bin path").trim_start(),
                None => rest,
            };
            rest.strip_prefix('"')
                .expect("a quoted bin path")
                .split('"')
                .next()
                .expect("a bin path")
                .to_string()
        }

        pub(super) fn remove(path: &str) {
            let wsl_exe = super::wsl_exe_path();
            smol::block_on(super::run_wsl_command(
                &wsl_exe,
                None,
                ["--exec", "rm", "-rf", "--", path],
                "e2e clean-up",
            ))
            .expect("removing what the end-to-end test staged");
        }

        /// 中身があるときは何もしない。本番と共用の場所を消さないための掃除。
        pub(super) fn remove_if_empty(path: &str) {
            let wsl_exe = super::wsl_exe_path();
            match smol::block_on(super::run_wsl_command(
                &wsl_exe,
                None,
                ["--exec", "rmdir", "--", path],
                "e2e clean-up",
            )) {
                Ok(output) if output.status.success() => {}
                Ok(_) => eprintln!("{path} is not empty, so it is left as it is"),
                Err(error) => eprintln!("could not remove {path}: {error:#}"),
            }
        }
    }

    /// 上の end-to-end が置く場所と、`e2e_zz_clean_up` が消す場所は、本物の Zed が
    /// 置いた場所と重ならない。重なっていた頃は、掃除がユーザーの staged Node.js を
    /// 消していた。E2E 自体には WSL が要るが、この隔離は綴りだけで決まるので、ここで
    /// 確かめられる。
    #[test]
    fn the_end_to_end_staging_never_reaches_what_a_real_zed_staged() {
        // 本物の Zed が置く場所 (`project` の `agent_server_store` の綴り)。
        let production = [
            "node/wsl-sandbox/node-v24.11.0-linux-x64",
            "external_agents/wsl-sandbox/registry/npx/zed-e2e",
        ];
        let root = e2e::root_relative();
        let staged = [e2e::node_relative(), e2e::install_relative()];
        assert_ne!(staged[0], staged[1]);

        for path in &staged {
            // 掃除は根ごと消すので、置くものが根の下にあることがそのまま安全の根拠。
            assert!(path.starts_with(&format!("{root}/")), "{path}");
            assert!(
                ensure_safe_relative_path(path, "end-to-end path").is_ok(),
                "{path}"
            );
            for production in production {
                assert!(!path.starts_with(production), "{path}");
                assert!(!production.starts_with(path.as_str()), "{path}");
            }
        }

        for accepted in ["zed-e2e", "e2e/run-1", "e2e-2"] {
            assert!(e2e::is_isolated_root(accepted), "{accepted}");
        }
        for refused in [
            "node",
            "node/e2e",
            "external_agents",
            "external_agents/wsl-sandbox/e2e",
            // 本番のものは `node` と `external_agents` だけではないので、E2E だと名乗らない
            // 根は受け付けない。
            "tmp",
            "db",
            "",
            "/absolute",
            "../outside",
            "e2e/../node",
        ] {
            assert!(!e2e::is_isolated_root(refused), "{refused}");
        }
    }

    /// (a) The pinned Linux Node.js is staged in the distro and runs *inside*
    /// the sandbox, reached through the one read grant it is given.
    #[test]
    #[ignore]
    fn e2e_a_the_linux_node_runs_inside_the_sandbox() {
        if !e2e::enabled() {
            return;
        }
        let node_directory = e2e::node_directory();
        assert!(node_directory.ends_with("node/wsl-sandbox/node-v24.11.0-linux-x64"));

        // ADR-F49 決定 25: a runtime whose only identity is the place it is in
        // — a registry archive published without a digest — is found there
        // under any marker, and a runtime staged under a different digest is
        // not mistaken for the one asked for.
        assert_eq!(
            smol::block_on(staged_wsl_runtime(None, &e2e::node_relative(), None))
                .expect("checking for any complete runtime"),
            Some(node_directory.clone())
        );
        assert_eq!(
            smol::block_on(staged_wsl_runtime(
                None,
                &e2e::node_relative(),
                Some(&"0".repeat(64))
            ))
            .expect("checking for another digest"),
            None
        );

        let (output, _sandbox) = e2e::run_in_sandbox(
            &format!("{node_directory}/bin/node"),
            &["--version"],
            &[],
            &[node_directory.as_str()],
            &[],
            None,
            None,
            true,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.trim() == "v24.11.0",
            "node --version printed {stdout:?} / {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// (b) An npm package installed by the sandboxed npm answers ACP's
    /// `initialize` when it is started, itself sandboxed, from that Node.js.
    #[test]
    #[ignore]
    fn e2e_b_the_installed_agent_answers_acp() {
        if !e2e::enabled() {
            return;
        }
        let node_directory = e2e::node_directory();
        let install_directory = smol::block_on(install_npm_package_in_wsl(
            None,
            &node_directory,
            e2e::PACKAGE_SPEC,
            &e2e::install_relative(),
            vec!["registry.npmjs.org".to_string()],
            None,
        ))
        .expect("installing the package inside the sandbox");

        let package_json = smol::block_on(read_wsl_npm_package_json(
            None,
            &install_directory,
            e2e::PACKAGE,
        ))
        .expect("reading the package.json")
        .expect("the package is installed");
        assert!(package_json.contains("\"name\""));
        let bin = e2e::bin_from_package_json(&package_json);
        let executable = smol::block_on(wsl_npm_package_executable(
            None,
            &install_directory,
            e2e::PACKAGE,
            &bin,
        ))
        .expect("the package's executable");

        let request = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false}}}}
"#;
        let (output, _sandbox) = e2e::run_in_sandbox(
            &format!("{node_directory}/bin/node"),
            &[executable.as_str()],
            &[],
            &[node_directory.as_str(), install_directory.as_str()],
            &[
                ("PATH", &format!("{node_directory}/bin:/usr/bin:/bin")),
                ("HOME", "/tmp"),
            ],
            None,
            Some(request),
            true,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("\"id\":1") && stdout.contains("protocolVersion"),
            "the agent answered {stdout:?} / {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// (c) A command registered as a sandbox session can be looked at and
    /// stopped from outside it.
    #[test]
    #[ignore]
    fn e2e_c_a_session_can_be_listed_and_stopped() {
        if !e2e::enabled() {
            return;
        }
        let session_id = "zed-e2e-session";
        let (_, sandbox) = e2e::run_in_sandbox(
            "/bin/sh",
            &["-c", "sleep 300 & sleep 300"],
            &[],
            &[],
            &[],
            Some(session_id),
            None,
            false,
        );

        let mut processes = Vec::new();
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            processes = smol::block_on(list_wsl_sandbox_processes(None, session_id))
                .expect("listing the session's processes");
            if processes.len() >= 2 {
                break;
            }
        }
        assert!(
            processes.len() >= 2,
            "the sandbox should hold the shell and its sleeps, not {processes:?}"
        );
        assert!(
            processes
                .iter()
                .any(|process| process.command.contains("sleep")),
            "{processes:?}"
        );

        let keys: Vec<_> = processes.iter().map(|process| process.key()).collect();
        let stopped = smol::block_on(stop_wsl_sandbox_processes(None, session_id, &keys))
            .expect("stopping the session's processes");
        assert!(stopped >= 1, "nothing was stopped");

        let mut left = Vec::new();
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            left = smol::block_on(list_wsl_sandbox_processes(None, session_id))
                .expect("listing the session's processes again");
            if left.is_empty() {
                break;
            }
        }
        assert!(left.is_empty(), "these were left running: {left:?}");
        drop(sandbox);
    }

    /// (d) An agent's configuration folder is made inside the distro, and a
    /// link left on the way to it fails the whole call.
    #[test]
    #[ignore]
    fn e2e_d_an_agent_config_folder_refuses_a_link() {
        if !e2e::enabled() {
            return;
        }
        let config = smol::block_on(ensure_wsl_agent_config_dir(
            None,
            e2e::PROJECT_KEY,
            "config",
            &["home".to_string()],
        ))
        .expect("making the agent's folder");
        assert!(config.ends_with(&format!(
            "{}/{}/config",
            crate::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE,
            e2e::PROJECT_KEY
        )));

        // Making it again changes nothing.
        let again = smol::block_on(ensure_wsl_agent_config_dir(
            None,
            e2e::PROJECT_KEY,
            "config",
            &["home".to_string()],
        ))
        .expect("making the agent's folder again");
        assert_eq!(config, again);

        // A level replaced by a link fails the whole call rather than putting
        // the agent's sign-in somewhere else.
        let wsl_exe = wsl_exe_path();
        let linked = format!("{config}/home");
        smol::block_on(run_wsl_command(
            &wsl_exe,
            None,
            [
                "--exec",
                "sh",
                "-c",
                "set -eu; rm -rf \"$1\"; ln -s /tmp \"$1\"",
                "zed-e2e-link",
                linked.as_str(),
            ],
            "e2e link",
        ))
        .expect("putting a link in the way");
        let refused = smol::block_on(ensure_wsl_agent_config_dir(
            None,
            e2e::PROJECT_KEY,
            "config",
            &["home".to_string()],
        ));
        let error = refused.expect_err("a link on the way must fail the call");
        assert!(
            format!("{error:#}").contains("link"),
            "the reason names the link: {error:#}"
        );
    }

    /// ADR-F49 第 2b 段, on a real distro: a worktree on the distribution's own
    /// ext4 — reached from Windows only as `\\wsl.localhost\<distro>\…` — is a
    /// write grant like any other. The whole path of one is exercised: the
    /// approval-time resolution inside WSL (following an ext4 symlink Windows
    /// cannot), and then a command wrapped with that grant, running with the
    /// worktree as its cwd, which must be able to write there and must not be
    /// able to read the account's credentials (decision 11) or write outside the
    /// worktree — even though the worktree lies inside that same home.
    ///
    /// It needs WSL and writes under the distro's home, so it runs only when
    /// asked for:
    ///
    /// ```text
    /// ZED_E2E_WSL_EXT4=1 cargo test -p sandbox --lib -- --ignored --nocapture ext4
    /// ```
    #[test]
    #[ignore]
    #[allow(
        clippy::disallowed_methods,
        reason = "an end-to-end driver, not the app: it waits on a real wsl.exe on its own thread"
    )]
    fn e2e_an_ext4_worktree_is_a_sandbox_root() {
        if !std::env::var("ZED_E2E_WSL_EXT4").is_ok_and(|value| value == "1") {
            return;
        }
        let wsl_exe = wsl_exe_path();
        let distro = std::env::var("ZED_E2E_WSL_DISTRO").ok();
        let distro_name = registered_wsl_distro_name(distro.as_deref())
            .expect("the distro the end-to-end test runs in is registered");
        let home = smol::block_on(query_distro_home(&wsl_exe, distro.as_deref()))
            .expect("the distro's home");
        let worktree = format!("{home}/zed-x2-e2e");
        let link = format!("{home}/zed-x2-e2e-link");

        let ext4_run = |what: &str, script: &str, argument: &str| -> std::process::Output {
            smol::block_on(run_wsl_command(
                &wsl_exe,
                distro.as_deref(),
                ["--exec", "sh", "-c", script, "zed-x2-e2e", argument],
                what,
            ))
            .unwrap_or_else(|error| panic!("{what}: {error:#}"))
        };

        // A worktree on ext4 with the two things a checkout has — a `.git` and
        // a source directory — plus a symlink to it (which only WSL can follow)
        // and a credential the sandbox must not be able to read. `.ssh` is left
        // exactly as it was found.
        let setup = ext4_run(
            "e2e setup",
            "set -eu; rm -rf \"$1\" \"$1-link\" \"$HOME/zed-x2-outside\"; \
             mkdir -p \"$1/src\" \"$1/.git/hooks\"; printf ref > \"$1/.git/config\"; \
             ln -s \"$1\" \"$1-link\"; \
             if [ -d \"$HOME/.ssh\" ]; then printf made-ssh=no; else mkdir -p \"$HOME/.ssh\"; \
             printf made-ssh=yes; fi; printf zed-x2-secret > \"$HOME/.ssh/zed-x2-probe\"",
            worktree.as_str(),
        );
        assert!(setup.status.success(), "the end-to-end setup runs");
        let made_ssh = String::from_utf8_lossy(&setup.stdout).contains("made-ssh=yes");

        let share = |linux_path: &str| {
            PathBuf::from(crate::util::wsl_share_path_spelling(
                &distro_name,
                linux_path,
            ))
        };
        // The approval-time resolution happens inside the distro: the symlink is
        // followed to the worktree, and what is stored keeps the share the grant
        // was named through.
        let resolved = smol::block_on(resolve_canonical_for_grant(
            share(&link),
            WslSessionDistro::named(&distro_name),
        ))
        .expect("resolving an ext4 grant");
        assert_eq!(resolved.canonical, share(&worktree));
        assert!(
            !resolved.on_windows_fs,
            "a worktree on the distro's own filesystem is not on a Windows drive"
        );
        // The same grant asked for as a bare Linux path names no distribution of
        // its own: it is resolved in the session's, and stored as that
        // distribution's share, so what the user approves is unambiguously the
        // folder the command will be able to write.
        let bare = smol::block_on(resolve_canonical_for_grant(
            PathBuf::from(&link),
            WslSessionDistro::named(&distro_name),
        ))
        .expect("resolving a bare Linux grant in the session's distro");
        assert_eq!(bare.canonical, share(&worktree));

        let mut sandbox = crate::Sandbox::new(crate::SandboxPolicy {
            fs: crate::SandboxFsPolicy::Restricted {
                writable_paths: vec![
                    crate::HostFilesystemLocation::reopen(share(&link), &resolved.canonical)
                        .expect("the resolved grant"),
                ],
                // The `.git` of each worktree, exactly as `acp_thread` names it
                // for a command the agent asks Zed to run.
                protected_paths: vec![
                    crate::HostFilesystemLocation::capture(share(&format!("{worktree}/.git")))
                        .expect("the worktree's .git"),
                ],
                readable_paths: Vec::new(),
            },
            network: crate::SandboxNetPolicy::Blocked,
        })
        .expect("a sandbox over an ext4 worktree");
        sandbox.set_wsl_linux_environment();
        let wrapped = smol::block_on(sandbox.wrap(&crate::CommandAndArgs {
            program: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                // Written inside the worktree: its root is carved around the
                // agent-configuration entries that are not there yet (decision
                // 8), the same on ext4 as anywhere else, so what proves the
                // grant is a write *in* the worktree, not one beside `.git`.
                "pwd > src/pwd.txt; touch src/ok; \
                 touch newentry 2>/dev/null \
                 && echo ROOT-WRITE-OK > src/root.txt || echo ROOT-WRITE-DENIED > src/root.txt; \
                 { cat \"$HOME/.ssh/zed-x2-probe\" && printf ' READ-OK'; } > src/ssh.txt 2>&1 \
                 || echo READ-DENIED > src/ssh.txt; \
                 touch \"$HOME/zed-x2-outside\" 2>/dev/null \
                 && echo WRITE-OK > src/outside.txt || echo WRITE-DENIED > src/outside.txt; \
                 printf pwned >> .git/config 2>/dev/null \
                 && echo WRITE-OK > src/git.txt || echo WRITE-DENIED > src/git.txt"
                    .to_string(),
                "zed-x2-e2e".to_string(),
            ],
            env: HashMap::default(),
            // The cwd is the worktree, named the only way Windows can name it.
            cwd: Some(share(&worktree)),
        }))
        .expect("wrapping a command over an ext4 worktree");
        let output = std::process::Command::new(&wrapped.program)
            .args(&wrapped.args)
            .envs(&wrapped.env)
            .output()
            .expect("running the wrapped command");
        assert!(
            output.status.success(),
            "the sandboxed command ran: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );

        let read_back = ext4_run(
            "e2e read-back",
            "set -eu; cd \"$1/src\"; printf 'ok=%s ' \"$(test -e ok && echo yes || echo no)\"; \
             printf 'pwd=%s ' \"$(cat pwd.txt)\"; printf 'ssh=%s ' \"$(cat ssh.txt)\"; \
             printf 'outside=%s ' \"$(cat outside.txt)\"; printf 'git=%s ' \"$(cat git.txt)\"; \
             printf 'root=%s ' \"$(cat root.txt)\"; \
             printf 'gitconfig=%s' \"$(cat ../.git/config)\"",
            worktree.as_str(),
        );
        let observed = String::from_utf8_lossy(&read_back.stdout).to_string();
        eprintln!("the ext4 worktree sandbox observed: {observed}");

        // Clean up before asserting, so a failure still leaves the distro as it
        // was found.
        ext4_run(
            "e2e clean-up",
            "rm -rf \"$1\" \"$1-link\" \"$HOME/zed-x2-outside\"; rm -f \"$HOME/.ssh/zed-x2-probe\"",
            worktree.as_str(),
        );
        if made_ssh {
            ext4_run(
                "e2e clean-up",
                "rmdir \"$HOME/.ssh\" 2>/dev/null || true",
                "",
            );
        }

        assert!(observed.contains("ok=yes"), "{observed}");
        assert!(observed.contains(&format!("pwd={worktree} ")), "{observed}");
        // Decision 11: the account's credentials are covered, even though the
        // worktree lies inside that same home.
        assert!(observed.contains("ssh=READ-DENIED"), "{observed}");
        assert!(!observed.contains("zed-x2-secret"), "{observed}");
        assert!(observed.contains("outside=WRITE-DENIED"), "{observed}");
        assert!(observed.contains("git=WRITE-DENIED"), "{observed}");
        // Decision 8, on ext4 as anywhere else: the worktree root is carved
        // around the agent-configuration entries that are not there yet, so a
        // new entry cannot be created beside `.git`.
        assert!(observed.contains("root=ROOT-WRITE-DENIED"), "{observed}");
        assert!(observed.contains("gitconfig=ref"), "{observed}");
    }

    /// Removes what the end-to-end tests staged in the distro. Everything they
    /// stage is below their own root, so this never reaches what a real Zed
    /// staged there — a staged Node.js is hundreds of megabytes to fetch again.
    #[test]
    #[ignore]
    fn e2e_zz_clean_up() {
        if !e2e::enabled() {
            return;
        }
        let data_directory = smol::block_on(wsl_agent_runtime_data_directory(None))
            .expect("the distro's data directory");
        e2e::remove(&format!("{data_directory}/{}", e2e::root_relative()));
        let home =
            smol::block_on(query_distro_home(&wsl_exe_path(), None)).expect("the distro's home");
        e2e::remove(&format!(
            "{home}/{}/{}",
            crate::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE,
            e2e::PROJECT_KEY
        ));
        // 作業フォルダーの根は本番と共用で、中身はインストールごとの一時フォルダー
        // (作った側が消す)。空のときだけ、つまり進行中のインストールが無いときだけ消す。
        e2e::remove_if_empty(&format!("{home}/{}", WSL_STAGING_HOME_RELATIVE));
    }
}
