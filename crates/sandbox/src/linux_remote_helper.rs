//! ADR-F18: the on-host sandbox helper that physically isolates an SSH-remote
//! Linux agent command.
//!
//! Bubblewrap cannot be used on Ubuntu 24.04 (`apparmor_restrict_unprivileged_userns=1`
//! refuses both the unprivileged-userns `uid_map` self-write and unprivileged
//! `mount`), so this backend builds the isolation by hand out of primitives that
//! *do* work there, verified on real 24.04 and 18.04 hosts:
//!
//! - **Network**: a `CLONE_NEWUSER|CLONE_NEWNET` "holder" that, by the time the
//!   command runs, has **nothing but its own loopback**. `slirp4netns` is used
//!   only to configure the namespace (an unprivileged user namespace confined by
//!   24.04's AppArmor cannot even bring its own `lo` up); it is started with
//!   `--outbound-addr=127.0.0.1 --disable-host-loopback` and then **killed**
//!   before the command is released, because its NAT can otherwise still reach
//!   any address the host itself owns. The one way out is a TCP listener the
//!   holder creates *inside* the namespace and hands to the parent over
//!   `SCM_RIGHTS`; the parent — which is outside the namespace — accepts on it
//!   and splices each connection to the host-loopback egress proxy
//!   (`http_proxy`) enforcing the per-command host allowlist. The command's
//!   `HTTP(S)_PROXY` points at that in-namespace listener, so the proxy's
//!   allowlist decision is the only reachable endpoint anywhere.
//! - **Filesystem**: **Landlock** where the kernel supports it (>= 5.13, so
//!   24.04) — writes confined to the worktree, everything else read-only; or an
//!   in-namespace **mount-jail** (`pivot_root` + read-only `/` + a read-write
//!   worktree bind) on older kernels (5.4, so 18.04). Either way a **seccomp**
//!   filter (shared with the Bubblewrap backend) blocks the command from ever
//!   creating an `AF_UNIX` socket.
//! - Anything missing (no `slirp4netns`, no user namespaces, neither FS backend)
//!   is **fail-closed**: the command never runs and the helper exits 126 with a
//!   machine-readable line on stderr the client turns into a Japanese error.
//!
//! # Namespace choreography (parent / holder split)
//!
//! Unprivileged `CLONE_NEWNET` is only permitted alongside `CLONE_NEWUSER`, and
//! writing a userns `uid_map` on 24.04 is only permitted *from the parent* (the
//! self-write is refused). So the parent `fork()`s a holder that `unshare`s the
//! user + network namespaces and blocks on a pipe handshake. The parent then
//! writes the holder's `uid_map`/`gid_map`, stands up the egress proxy and
//! `slirp4netns`, and releases the holder (handing it the proxy port).
//!
//! The holder does **not** re-`exec` a second helper stage before jailing the
//! filesystem: doing so would drop the user-namespace capabilities the mount
//! jail needs. Because the parent starts no thread before the `fork` (the proxy
//! is started afterwards), the holder inherits a single-threaded address space
//! and can run ordinary Rust — applying the FS backend (Landlock or mount jail)
//! and the seccomp filter itself, then finally `exec`ing the untrusted command
//! (the only point at which its capabilities drop).
//!
//! # Signals and leftovers (ADR-F49, ADR-F50)
//!
//! The parent outlives the command in every mode: it owns the egress and IDE
//! relays and the scratch directories. It ignores SIGINT / SIGQUIT / SIGTSTP —
//! a terminal sends those to its whole foreground process group, the command
//! included — and passes SIGHUP / SIGTERM on to the command. The command runs
//! in a PID namespace of its own, under an init the holder forks (see
//! [`enter_command_namespaces`]): when that init exits the kernel stops
//! everything left in the namespace, daemons that `setsid` away included. A
//! host that refuses the PID namespace fails closed (`pidns-failed`) today.
//! The parent is also a `PR_SET_CHILD_SUBREAPER`, so whatever still ends up
//! reparented to it is killed once the command has exited.
//!
//! # IDE forward (ADR-F49)
//!
//! For an interactive session the holder also listens on a fixed port of the
//! namespace's loopback, and the parent splices each connection to the IDE
//! bridge's Unix socket (see `crate::ide_bridge`), which must lie in a
//! directory the command cannot write to.

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::RemoteSandboxNetwork;
use crate::linux_bubblewrap::install_command_seccomp_filter;
use crate::protected_paths::{
    SANDBOXED_AGENT_CONFIG_HOME_RELATIVE, UNIX_PROTECTED_HOME_RELATIVE,
    WSL_SANDBOX_HELPER_HOME_RELATIVE, account_from_passwd, app_state_directories_from_environment,
    around_reachable, git_config_directory_from_environment, home_dir_from_account,
    is_sandboxed_agent_configuration, merge_remote_builtin_protected_paths, read_hidden_home_paths,
    remote_agent_runtime_data_directory, resolve_readable_path, worktree_entries,
    zsh_startup_files_from_environment,
};
use crate::util::CanonicalPathBuf;
use crate::{
    IDE_BRIDGE_FLAG, REMOTE_SANDBOX_HELPER_FLAG, WSL_HELPER_MODE_FLAG, WSL_PROBE_RESULT_PREFIX,
    WSL_SANDBOX_PROBE_FLAG, WSL_SANDBOX_RESOLVE_FLAG,
};

/// Exit code for "the sandbox could not be built" (mirrors the Bubblewrap
/// backend). The client keys its Japanese fail-closed error off this.
const SANDBOX_SETUP_FAILED_EXIT_CODE: i32 = 126;

/// Basename of the bundled slirp4netns binary installed next to the remote
/// server binary in `~/.zed_server`.
const SLIRP_BINARY_NAME: &str = "slirp4netns";

/// Marker opening the fail-closed stderr line for an SSH host.
const REMOTE_FAILURE_MARKER: &str = "zed-remote-sandbox";

/// Marker opening the fail-closed stderr line in the local WSL mode. The codes
/// are shared with the SSH host, but the fixes are not (`wsl --update` rather
/// than asking a server administrator), so the client needs to tell them apart.
const WSL_FAILURE_MARKER: &str = "zed-wsl-sandbox";

/// Set once the invocation is known to come from the local WSL backend, so
/// [`fail_closed`] (which also runs in the forked holder) picks its marker.
static WSL_MODE: AtomicBool = AtomicBool::new(false);

// ===========================================================================
// Entry point
// ===========================================================================

/// Handle a possible re-exec of this binary as the remote sandbox helper, as
/// one of the two short-lived WSL utilities the Windows backend runs
/// ([`WSL_SANDBOX_RESOLVE_FLAG`], [`WSL_SANDBOX_PROBE_FLAG`]), or as the IDE
/// bridge ([`IDE_BRIDGE_FLAG`], ADR-F49). None of them returns if it matched.
/// Called from [`crate::run_remote_sandbox_helper_if_invoked`] at the top of
/// `main`.
pub(crate) fn run_if_invoked() {
    let args: Vec<OsString> =
        crate::expand_remote_sandbox_helper_argv(std::env::args_os().skip(1).collect());
    let Some((flag, rest)) = args.split_first() else {
        return;
    };
    if flag != WSL_SANDBOX_RESOLVE_FLAG
        && flag != WSL_SANDBOX_PROBE_FLAG
        && flag != REMOTE_SANDBOX_HELPER_FLAG
        && flag != IDE_BRIDGE_FLAG
    {
        return;
    }
    close_verified_exec_descriptor();
    if flag == IDE_BRIDGE_FLAG {
        crate::ide_bridge::run_ide_bridge(rest);
    }
    if flag == WSL_SANDBOX_RESOLVE_FLAG {
        run_wsl_resolve(rest);
    }
    if flag == WSL_SANDBOX_PROBE_FLAG {
        run_wsl_probe();
    }
    // Decided before parsing so that even a malformed WSL invocation reports
    // under the WSL marker.
    WSL_MODE.store(requests_wsl_mode(rest), Ordering::Relaxed);
    let mut invocation = match parse_stage1_args(rest.iter().cloned()) {
        Ok(invocation) => invocation,
        Err(error) => fail_closed("bad-args", &format!("{error:#}")),
    };
    WSL_MODE.store(invocation.wsl, Ordering::Relaxed);
    invocation.on_wsl = invocation.wsl || running_on_wsl();
    run_helper(invocation);
}

/// The Windows backend starts the helper through `/proc/self/fd/<n>` of the
/// file it has just hashed, so that what runs is exactly what was checked; that
/// descriptor stays open in this process. Close it rather than hand it on to
/// the sandboxed command.
fn close_verified_exec_descriptor() {
    use std::os::unix::fs::MetadataExt as _;
    let Some(program) = std::env::args_os().next() else {
        return;
    };
    let Some(fd) = program
        .to_str()
        .and_then(|program| program.strip_prefix("/proc/self/fd/"))
        .and_then(|number| number.parse::<RawFd>().ok())
    else {
        return;
    };
    if fd <= 2 {
        return;
    }
    let is_this_executable = match (
        crate::util::linux_fd_identity(fd),
        std::fs::metadata("/proc/self/exe"),
    ) {
        (Some(identity), Ok(metadata)) => identity == (metadata.dev(), metadata.ino()),
        _ => false,
    };
    // SAFETY: the descriptor was inherited from the verifying shell for this
    // alone, and nothing in this process has taken ownership of it.
    if is_this_executable && unsafe { libc::close(fd) } != 0 {
        eprintln!(
            "zed-remote-sandbox: could not close descriptor {fd}: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Whether this is WSL's kernel, read from the kernel itself (its interop
/// `binfmt_misc` entry and release string) rather than from the environment,
/// which the caller controls. WSL runs commands as root by default and hands
/// every process a Windows interop socket, so the helper hardens itself there
/// whichever transport started it — a WSL-remote project comes in over the SSH
/// protocol, without `--wsl`.
fn running_on_wsl() -> bool {
    Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
        || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop-late").exists()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// Whether the policy flags (everything before the command's `--`) select the
/// local WSL mode.
fn requests_wsl_mode(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_os_str() != OsStr::new("--"))
        .any(|arg| arg.as_os_str() == OsStr::new(WSL_HELPER_MODE_FLAG))
}

/// Route this crate's `log` records to stderr, but **only** when
/// `ZED_REMOTE_SANDBOX_LOG` is set in the environment.
///
/// The helper runs inside the remote server binary before any logger exists, so
/// its diagnostics (the effective protected list, paths it declined to grant)
/// would otherwise go nowhere. They cannot be printed unconditionally either:
/// the helper's stderr *is* the agent command's stderr, and the model reads it.
/// Off by default, one env var away when a host needs diagnosing.
struct HelperLogger;

impl log::Log for HelperLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("zed-remote-sandbox[{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

static HELPER_LOGGER: HelperLogger = HelperLogger;

pub(crate) fn init_helper_logging() {
    if std::env::var_os("ZED_REMOTE_SANDBOX_LOG").is_none() {
        return;
    }
    if log::set_logger(&HELPER_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

/// Emit the structured, machine-readable failure line the client parses and exit
/// fail-closed. Never returns. `code` is a stable token; `detail` is human text.
fn fail_closed(code: &str, detail: &str) -> ! {
    let marker = if WSL_MODE.load(Ordering::Relaxed) {
        WSL_FAILURE_MARKER
    } else {
        REMOTE_FAILURE_MARKER
    };
    eprintln!("{marker}: {code}: {detail}");
    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
}

// ===========================================================================
// Stage 1 (parent): decode, capabilities, fork holder, maps, slirp, proxy
// ===========================================================================

/// A decoded stage-1 invocation.
struct Stage1Invocation {
    network: RemoteSandboxNetwork,
    /// The editor-wide network allowlist `network` is nested inside, which only
    /// the local WSL backend sends (an SSH host gets none).
    network_ceiling: Option<Arc<crate::EditorAllowlistCeiling>>,
    allow_fs_write: bool,
    writable_paths: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
    /// ADR-F49 decision 14: what the command may read among the read-hidden
    /// entries (`--readable`), as the client named it; checked, and resolved,
    /// before anything runs (see [`resolve_readable_paths`]).
    readable_paths: Vec<PathBuf>,
    chdir: Option<PathBuf>,
    /// ADR-F46: invoked by the local Windows backend inside WSL (see
    /// [`WSL_HELPER_MODE_FLAG`] for what changes).
    wsl: bool,
    /// Running on WSL's kernel, in either mode: the hardening WSL needs applies
    /// whoever invoked the helper (see [`running_on_wsl`]).
    on_wsl: bool,
    /// SHA-256 of the `slirp4netns` the Windows backend installed beside this
    /// binary, checked before it runs (see [`verify_slirp`]).
    slirp_sha256: Option<String>,
    /// Variables to set in the command's environment. `wsl.exe` does not carry
    /// the Windows environment across, so the WSL backend passes it here.
    set_environment: Vec<(OsString, OsString)>,
    /// Variables to remove from the command's environment.
    unset_environment: Vec<OsString>,
    /// ADR-F49: a long-running program on a terminal (`--interactive`), whose
    /// standard streams are the terminal's and are handed to it as they are.
    /// The signal policy and the clean-up are the same for every command.
    interactive: bool,
    /// ADR-F49: the IDE channel of an interactive session.
    ide_forward: Option<IdeForwardRequest>,
    program: OsString,
    args: Vec<OsString>,
}

/// `--ide-listen` and `--ide-rendezvous` (ADR-F49).
#[derive(Debug, PartialEq)]
struct IdeForwardRequest {
    /// The port the holder listens on inside the network namespace.
    port: u16,
    /// The IDE bridge's Unix socket, outside the sandbox.
    rendezvous: PathBuf,
}

fn parse_stage1_args(args: impl IntoIterator<Item = OsString>) -> Result<Stage1Invocation> {
    let mut net_mode: Option<String> = None;
    let mut allowed_hosts: Vec<String> = Vec::new();
    let mut ceiling_entries: Option<Vec<String>> = None;
    let mut allow_fs_write = false;
    let mut writable_paths = Vec::new();
    let mut protected_paths = Vec::new();
    let mut readable_paths = Vec::new();
    let mut chdir = None;
    let mut wsl = false;
    let mut slirp_sha256 = None;
    let mut set_environment = Vec::new();
    let mut unset_environment = Vec::new();
    let mut interactive = false;
    let mut ide_listen: Option<u16> = None;
    let mut ide_rendezvous: Option<PathBuf> = None;
    let mut program = None;
    let mut trailing = Vec::new();

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let flag = arg.to_str().unwrap_or_default();
        match flag {
            "--net" => {
                let value = args.next().context("--net requires a value")?;
                net_mode = Some(value.to_string_lossy().into_owned());
            }
            "--allow-host" => {
                let value = args.next().context("--allow-host requires a value")?;
                allowed_hosts.push(value.to_string_lossy().into_owned());
            }
            "--network-ceiling" => {
                ceiling_entries.get_or_insert_with(Vec::new);
            }
            "--ceiling-host" => {
                let value = args.next().context("--ceiling-host requires a value")?;
                ceiling_entries
                    .get_or_insert_with(Vec::new)
                    .push(value.to_string_lossy().into_owned());
            }
            "--allow-fs-write" => allow_fs_write = true,
            "--writable" => {
                let value = args.next().context("--writable requires a value")?;
                writable_paths.push(PathBuf::from(value));
            }
            "--protected" => {
                let value = args.next().context("--protected requires a value")?;
                protected_paths.push(PathBuf::from(value));
            }
            "--readable" => {
                let value = args.next().context("--readable requires a value")?;
                readable_paths.push(PathBuf::from(value));
            }
            "--chdir" => {
                let value = args.next().context("--chdir requires a value")?;
                chdir = Some(PathBuf::from(value));
            }
            WSL_HELPER_MODE_FLAG => wsl = true,
            "--slirp-sha256" => {
                let value = args.next().context("--slirp-sha256 requires a value")?;
                slirp_sha256 = Some(value.to_string_lossy().into_owned());
            }
            "--setenv" => {
                let name = args.next().context("--setenv requires a name")?;
                let value = args.next().context("--setenv requires a value")?;
                ensure_environment_name(&name)?;
                set_environment.push((name, value));
            }
            "--unsetenv" => {
                let name = args.next().context("--unsetenv requires a name")?;
                ensure_environment_name(&name)?;
                unset_environment.push(name);
            }
            "--interactive" => interactive = true,
            "--ide-listen" => {
                let value = args.next().context("--ide-listen requires a port")?;
                let port = value
                    .to_str()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|port| *port != 0)
                    .with_context(|| format!("invalid --ide-listen port {value:?}"))?;
                ide_listen = Some(port);
            }
            "--ide-rendezvous" => {
                let value = args.next().context("--ide-rendezvous requires a path")?;
                let path = PathBuf::from(value);
                ensure!(
                    path.is_absolute(),
                    "--ide-rendezvous must be an absolute path, not {}",
                    path.display()
                );
                ide_rendezvous = Some(path);
            }
            "--" => {
                program = args.next();
                trailing = args.collect();
                break;
            }
            other => bail!("unknown remote sandbox helper flag: {other}"),
        }
    }

    let network = match net_mode.as_deref() {
        Some("blocked") | None => RemoteSandboxNetwork::Blocked,
        Some("any") => RemoteSandboxNetwork::AnyHost,
        Some("hosts") => RemoteSandboxNetwork::Hosts(allowed_hosts),
        Some(other) => bail!("unknown --net mode: {other}"),
    };
    let network_ceiling = ceiling_entries
        .map(|entries| crate::EditorAllowlistCeiling::parse(&entries).map(Arc::new))
        .transpose()?;
    let program = program.context("missing program after `--`")?;
    let ide_forward = match (ide_listen, ide_rendezvous) {
        (Some(port), Some(rendezvous)) => {
            ensure!(
                interactive,
                "an IDE forward is only for an --interactive session"
            );
            Some(IdeForwardRequest { port, rendezvous })
        }
        (None, None) => None,
        _ => bail!("--ide-listen and --ide-rendezvous go together"),
    };

    Ok(Stage1Invocation {
        network,
        network_ceiling,
        allow_fs_write,
        writable_paths,
        protected_paths,
        readable_paths,
        chdir,
        wsl,
        on_wsl: wsl,
        slirp_sha256,
        set_environment,
        unset_environment,
        interactive,
        ide_forward,
        program,
        args: trailing,
    })
}

/// Reject a name `setenv(3)` would refuse, here rather than in the holder, where
/// `std::env::set_var` would panic on it.
fn ensure_environment_name(name: &OsStr) -> Result<()> {
    if name.is_empty() || name.as_bytes().contains(&b'=') {
        bail!("invalid environment variable name {name:?}");
    }
    Ok(())
}

/// Whether outbound access is requested at all (Blocked leaves the netns empty).
fn network_requested(network: &RemoteSandboxNetwork) -> bool {
    !matches!(network, RemoteSandboxNetwork::Blocked)
}

#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              waiting on the holder and slirp children with blocking APIs is correct here"
)]
fn run_helper(mut invocation: Stage1Invocation) -> ! {
    init_helper_logging();
    // Before anything opens a descriptor: whatever started the helper may have
    // left one of the standard three closed, and the next `pipe2` would then
    // take its number. The client's end would be a pipe of our own, which the
    // hangup watch below would see close and take for the client going away.
    let standard_streams = match ensure_standard_descriptors() {
        Ok(streams) => streams,
        Err(error) => fail_closed("fd-failed", &format!("{error:#}")),
    };
    // Reclaim scratch directories from earlier runs that died before their own
    // cleanup. Cheap, and these hosts have small disks.
    sweep_stale_temp_dirs();

    // --- Capability gate (fail-closed before running anything) -------------
    let fs_backend = match select_fs_backend(invocation.on_wsl) {
        Ok(backend) => backend,
        Err(error) => fail_closed("no-fs-backend", &format!("{error:#}")),
    };

    // Add the protected paths the client cannot know about (see
    // `crate::protected_paths`). Resolved here in the parent, where the
    // environment is still the user's ssh session's, and before the fork so the
    // holder pins them along with the client's `.git` list.
    // The account database, never `$HOME`: this process's environment can be
    // shaped by others — `WSLENV` from the Windows side, a dev container's
    // `containerEnv` — and a `HOME` from there would aim the protected set
    // anywhere.
    let Some(home) = home_dir_from_account() else {
        fail_closed(
            "home-unknown",
            "neither $HOME nor the passwd database names an absolute home directory, \
             so the paths that must always be protected cannot be determined",
        );
    };
    // The home the read hiding applies to: the account's own, or the fake one
    // the test helper points it at so that no real home is touched.
    let hidden_home = read_hidden_home(&home);

    // Pinned before anything is decided about them: what the client named is
    // only a name until it is pinned, and every decision below — which roots
    // are a worktree, what stays reachable among the hidden entries, what is
    // covered — is about where a root really is.
    let original_open_file_limit = raise_open_file_limit();
    let writable_pins = match pin_writable_paths(&invocation.writable_paths, invocation.wsl) {
        Ok(pins) => pins,
        Err(error) => fail_closed("writable-redirected", &format!("{error:#}")),
    };

    invocation.protected_paths =
        merge_remote_builtin_protected_paths(invocation.protected_paths, &home);
    // The client names each root's `.git`; the agent configurations beside it
    // (ADR-F49) come from the roots the command may write, so that no client
    // can forget them — but not in a sandboxed agent's own configuration
    // directory, which no host agent reads (decision 15). The environment can
    // only add protection here.
    let additions = zsh_startup_files_from_environment()
        .into_iter()
        .chain(git_config_directory_from_environment())
        .chain(worktree_protected_additions(
            &invocation,
            &writable_pins,
            &hidden_home,
        ));
    for path in additions {
        if !invocation.protected_paths.contains(&path) {
            invocation.protected_paths.push(path);
        }
    }
    // The sandboxed agents' configuration: a hook or an `apiKeyHelper` written
    // in another project's directory runs in that project's next session, with
    // its grants — a command that can write there has injected code into it.
    // Protected on every host (decision 15 puts these directories on SSH hosts
    // and local machines too), and not only where the read hiding can cover
    // them: on a host that refuses the mounts this is all there is. A session's
    // own directory is inside it and comes from its write grant, which this
    // does not narrow (see [`holds_writable_root`]).
    // Where the enclosure around it is, so that the protection and the one
    // root it steps aside for are decided about the same directory.
    let agent_configs = hidden_home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
    if !invocation.protected_paths.contains(&agent_configs) {
        invocation.protected_paths.push(agent_configs);
    }
    if invocation.on_wsl {
        // Where the Windows side installs this very binary, stages agent
        // binaries, and keeps the IDE bridge's sockets and lock files:
        // overwriting any of it runs code outside the sandbox on the next
        // command (the WSL counterpart of `.zed_server`, which the list above
        // already covers). Protected whenever the helper runs on a WSL kernel,
        // `--wsl` or not — a WSL-remote project comes in over the SSH protocol
        // without `--wsl` yet shares the same install root.
        let install_root = home.join(WSL_SANDBOX_HELPER_HOME_RELATIVE);
        if !invocation.protected_paths.contains(&install_root) {
            invocation.protected_paths.push(install_root);
        }
    }
    // ADR-F49 decision 11: what the command may not read, taken here from the
    // environment the caller gave, like the protected set above; init covers
    // them once the filesystem backend's own mounts are in place, split around
    // the roots the command has to keep reaching.
    // ADR-F49 decision 14: what the client asked to show among those entries,
    // checked before anything runs, so that one that may not be shown fails
    // the command rather than being dropped, and pinned like a write grant so
    // the cover binds back the very object that was checked.
    let data_directory = remote_agent_runtime_data_directory(&hidden_home);
    let readable =
        match resolve_readable_paths(&invocation.readable_paths, &data_directory, &hidden_home) {
            Ok(readable) => readable,
            Err(error) => fail_closed("readable-refused", &format!("{error:#}")),
        };
    let readable_pins = match pin_readable_paths(&readable) {
        Ok(pins) => pins,
        Err(error) => fail_closed("readable-refused", &format!("{error:#}")),
    };
    let reachable = reachable_paths(&invocation, &home);
    let enclosures = enclosed_directories(&hidden_home, &data_directory);
    let mut read_hidden = read_hidden_home_paths(&hidden_home, false);
    for directory in app_state_directories_from_environment() {
        if !read_hidden.contains(&directory) {
            read_hidden.push(directory);
        }
    }
    // Decided here, where the command can still be refused: what the command is
    // shown must not overlap what the hiding covers. Leaving the cover off
    // instead — which is what dropping the target would do — would hand it a
    // credential store, and for good: the entry would stay uncovered for every
    // command that follows.
    let (_, targets) = read_hiding_plan(
        &read_hidden,
        &reachable,
        &enclosures,
        &writable_pins,
        &readable_pins,
    );
    let shown: Vec<PathBuf> = readable_pins
        .iter()
        .map(|pin| pin.path().to_path_buf())
        .chain(
            reachable
                .iter()
                .filter(|path| !invocation.writable_paths.contains(path))
                .cloned(),
        )
        .collect();
    if let Err(error) = check_nothing_shown_is_covered(&shown, &targets) {
        fail_closed("read-hiding-overlap", &format!("{error:#}"));
    }
    log::info!(
        "[remote-sandbox] protecting {} paths ({} built-ins under HOME {})",
        invocation.protected_paths.len(),
        UNIX_PROTECTED_HOME_RELATIVE.len(),
        home.display()
    );

    let self_exe = std::env::current_exe().unwrap_or_default();
    let network_on = network_requested(&invocation.network);
    // The egress listener and the IDE listener both need the namespace's
    // loopback up, and only slirp4netns can bring it up there.
    let loopback_needed = network_on || invocation.ide_forward.is_some();
    let slirp = if loopback_needed {
        let Some(path) = locate_slirp4netns(&self_exe) else {
            fail_closed(
                "slirp-missing",
                &format!(
                    "{SLIRP_BINARY_NAME} was not found next to {}",
                    self_exe.display()
                ),
            );
        };
        match verify_slirp(path, invocation.slirp_sha256.as_deref()) {
            Ok(slirp) => Some(slirp),
            Err(error) => fail_closed("helper-mismatch", &format!("{error:#}")),
        }
    } else {
        None
    };

    // Planned here, before the fork, so that this process holds every inode the
    // holder relies on until the command exits (see `FilesystemPlan`); the
    // roots were pinned at the very start, for the same reason.
    let protected = match classify_protected_paths(&invocation.protected_paths) {
        Ok(entries) => entries,
        Err(error) => fail_closed("protected-pin-failed", &format!("{error:#}")),
    };
    let plan = match fs_backend {
        FsBackend::Landlock(_) => match plan_filesystem(
            &writable_pins,
            &protected,
            invocation.allow_fs_write,
            invocation.wsl,
            &directory_names_are_exact,
        ) {
            Ok(plan) => plan,
            Err(error) => fail_closed("landlock-failed", &format!("{error:#}")),
        },
        FsBackend::MountJail => FilesystemPlan::default(),
    };

    let ide_rendezvous = match &invocation.ide_forward {
        Some(forward) => match IdeRendezvous::open(&forward.rendezvous, &protected, &writable_pins)
        {
            Ok(rendezvous) => Some(rendezvous),
            Err(error) => fail_closed("ide-forward-invalid", &format!("{error:#}")),
        },
        None => None,
    };
    let ide_port = invocation
        .ide_forward
        .as_ref()
        .map_or(0, |forward| forward.port);

    // Before the fork, so that whatever the command leaves behind is ours to
    // stop from its very start.
    if let Err(error) = become_child_subreaper() {
        fail_closed("subreaper-failed", &format!("{error:#}"));
    }
    // Held back until the fork is done and the parent knows whom to pass them
    // on to. Every thread the parent starts later inherits the block, which
    // leaves them to the thread that waits for the command.
    let original_signal_mask = match block_command_signals() {
        Ok(mask) => mask,
        Err(error) => fail_closed("signals-failed", &format!("{error:#}")),
    };

    // Handshake pipes. Crucially, no thread — in particular the egress proxy — is
    // started before the fork, so the holder inherits a single-threaded address
    // space and may run ordinary Rust after the fork. That matters because the
    // holder applies the FS backend itself instead of `exec`ing a second helper
    // stage: an `exec` would drop the user-namespace capabilities the mount jail
    // needs. `go` doubles as the channel that hands the holder the proxy port.
    let ready = match Pipe::new() {
        Ok(pipe) => pipe,
        Err(error) => fail_closed("pipe-failed", &error.to_string()),
    };
    let go = match Pipe::new() {
        Ok(pipe) => pipe,
        Err(error) => fail_closed("pipe-failed", &error.to_string()),
    };
    // Channel for the holder to hand back the listening socket it creates inside
    // the network namespace (an fd can only travel over `SCM_RIGHTS`, so this is
    // a socketpair rather than a pipe).
    let relay = match SocketPair::new() {
        Ok(pair) => pair,
        Err(error) => fail_closed("pipe-failed", &error.to_string()),
    };
    // The command's standard streams (ADR-F49 decision 9): the parent's own
    // pipes, so the client's end stays out of the sandbox.
    let stdio_relay = match StdioRelay::new(&invocation, standard_streams) {
        Ok(relay) => relay,
        Err(error) => fail_closed("pipe-failed", &format!("{error:#}")),
    };
    let ready_write_fd = ready.write.as_raw_fd();
    let go_read_fd = go.read.as_raw_fd();
    let relay_holder_fd = relay.holder.as_raw_fd();
    let terminal = foreground_terminal();
    // SAFETY: `getpid` takes no arguments and cannot fail.
    let parent_pid = unsafe { libc::getpid() };

    // SAFETY: a single `fork` from a single-threaded process. The holder becomes
    // its own process; it and the parent coordinate only through the pipes.
    let child_pid = unsafe { libc::fork() };
    if child_pid < 0 {
        fail_closed("fork-failed", "fork() failed");
    }
    if child_pid == 0 {
        // Holder. Never returns. (`invocation` and `fs_backend` are moved in only
        // on this diverging path, so the parent below still owns them.)
        // The command's end of the relay first, so that nothing that runs from
        // here on — the holder, init, the command — holds the client's own
        // descriptors.
        if let Err(error) = stdio_relay.enter_command_side() {
            fail_closed("pipe-failed", &format!("{error:#}"));
        }
        run_holder(
            HolderSetup {
                invocation,
                fs_backend,
                writable_pins: &writable_pins,
                readable_pins: &readable_pins,
                protected: &protected,
                plan: &plan,
                original_open_file_limit,
                original_signal_mask,
                parent_pid,
                terminal: terminal.map(|(descriptor, _)| descriptor),
                read_hidden: &read_hidden,
                reachable: &reachable,
                enclosures: &enclosures,
            },
            ready_write_fd,
            go_read_fd,
            relay_holder_fd,
        );
    }
    let stdio_relay = stdio_relay.into_parent_side();

    // --- Parent -----------------------------------------------------------
    if let Err(error) = install_command_signal_policy(child_pid) {
        abort_holder(child_pid, "signals-failed", &format!("{error:#}"));
    }
    // Close the ends we don't use so the holder's copies become the sole
    // remaining ones (EOF semantics if it dies).
    drop(ready.write);
    drop(go.read);
    drop(relay.holder);
    let ready_read = ready.read;
    let go_write = go.write;
    let relay_parent = relay.parent;

    // Wait for the holder to report it created the namespaces (a non-OK byte or
    // EOF means it couldn't).
    match read_one_byte(ready_read.as_raw_fd()) {
        Ok(HOLDER_OK) => {}
        Ok(HOLDER_NO_PID_NAMESPACE) => abort_holder(
            child_pid,
            "pidns-failed",
            "the host refused a PID namespace for the command, without which it could \
             signal the helper and outlive it",
        ),
        _ => abort_holder(
            child_pid,
            "userns-failed",
            "the remote host refused to create a user+network namespace \
             (unprivileged user namespaces may be disabled)",
        ),
    }

    // Map the holder's uid/gid to our real ids so worktree files (owned by us)
    // stay writable and the command doesn't run as `nobody`.
    if let Err(error) = write_id_maps(child_pid) {
        abort_holder(child_pid, "uidmap-failed", &format!("{error:#}"));
    }

    // Stand up the egress proxy (host loopback) now — after the fork, so its
    // threads never entered the holder — then attach slirp to the holder's
    // network namespace.
    let proxy = if network_on {
        match start_egress_proxy(&invocation.network, invocation.network_ceiling.as_ref()) {
            Ok(proxy) => Some(proxy),
            Err(error) => abort_holder(child_pid, "proxy-failed", &format!("{error:#}")),
        }
    } else {
        None
    };
    let proxy_port = proxy.as_ref().map(|proxy| proxy.port()).unwrap_or(0);

    let mut slirp_child = match slirp
        .as_ref()
        .map(|slirp| start_slirp(slirp, child_pid))
        .transpose()
    {
        Ok(child) => child,
        Err(error) => abort_holder(child_pid, "slirp-failed", &format!("{error:#}")),
    };

    // Release the holder with the ports it is to listen on (0 = none).
    if let Err(error) = write_all_bytes(
        go_write.as_raw_fd(),
        &encode_holder_release(proxy_port, ide_port),
    ) {
        abort_holder(child_pid, "handshake-failed", &error.to_string());
    }

    // Take over the listening sockets the holder created inside the network
    // namespace, start serving them, and only then let the command run.
    if loopback_needed {
        let listeners_arrived = take_over_namespace_listeners(
            relay_parent.as_raw_fd(),
            ide_rendezvous,
            network_on.then_some(proxy_port),
            child_pid,
        );
        // slirp has now done its one remaining job: configuring the namespace
        // (bringing its loopback up) so the listeners above could be bound.
        // Stop it before the command starts. Even with `--disable-host-loopback`
        // its NAT can still reach any address the *host itself* owns — a socket
        // bound to `127.0.0.1` is free to connect to the host's own LAN address,
        // which is how `192.168.x.x:22` answered from inside the sandbox. With
        // slirp gone the namespace keeps nothing but its own loopback, so the
        // relays are the only way out.
        if let Some(slirp_process) = slirp_child.as_mut() {
            stop_slirp(slirp_process);
        }
        // Release the holder only now, so the command can never observe an
        // outbound path that is about to be taken away. A listener that did not
        // arrive means the holder failed closed on its own and printed its
        // reason; it exits 126, which the wait below passes on.
        if listeners_arrived && let Err(error) = write_all_bytes(go_write.as_raw_fd(), &[HOLDER_OK])
        {
            abort_holder(child_pid, "handshake-failed", &error.to_string());
        }
    }
    drop(slirp_child);
    drop(go_write);
    drop(relay_parent);

    // Started with the signals still blocked, as the relays are.
    let stdio_relay = match stdio_relay.start() {
        Ok(relay) => relay,
        Err(error) => abort_holder(
            child_pid,
            "pipe-failed",
            &format!("relaying the command's standard streams: {error}"),
        ),
    };
    if let Err(error) = watch_for_client_hangup(standard_streams) {
        abort_holder(
            child_pid,
            "signals-failed",
            &format!("watching for the client's hangup: {error}"),
        );
    }

    // From here on this thread takes the signals held back since the fork; the
    // relay and proxy threads started above keep them blocked.
    restore_signal_mask(&original_signal_mask);
    let status = wait_for_command(child_pid);
    if let Some((descriptor, group)) = terminal {
        reclaim_terminal(descriptor, group);
    }
    terminate_remaining_descendants();
    // Nothing of the command is left to write, so what is still on its way to
    // the client is all there will be: hand it over before exiting.
    stdio_relay.wait_until_drained();
    // Only now; see `FilesystemPlan` for why the parent holds these open.
    drop(plan);
    drop(protected);
    drop(writable_pins);

    drop(slirp);
    drop(proxy);
    // Init created the scratch directories under the holder's pid, and neither
    // outlives the command, so we are the one left to remove them — on every
    // path, including the failing ones below.
    remove_command_temp_dirs(child_pid);

    match status {
        Ok(code) => std::process::exit(code),
        Err(_) => fail_closed("wait-failed", "failed to wait for the sandboxed command"),
    }
}

/// Stop and reap the holder — it has not started the command yet wherever
/// this is called — stop anything else still running (slirp4netns may be),
/// remove what it left under `/tmp`, and fail closed. Every post-`fork` parent
/// failure goes through here so no path can leak the per-command scratch
/// directories. Never returns.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              blocking on the holder is correct here"
)]
fn abort_holder(child_pid: libc::pid_t, code: &str, detail: &str) -> ! {
    // A holder blocked on the handshake would wait for its release forever: it
    // holds its own copy of the pipe's write end, so it never sees EOF.
    // SAFETY: `kill` with scalar arguments; the holder is our unreaped child,
    // so the pid is still its own.
    if unsafe { libc::kill(child_pid, libc::SIGKILL) } != 0 {
        log::debug!(
            "[remote-sandbox] stopping the holder: {}",
            std::io::Error::last_os_error()
        );
    }
    if let Err(error) = wait_pid(child_pid) {
        log::warn!("[remote-sandbox] could not reap the holder: {error:#}");
    }
    terminate_remaining_descendants();
    remove_command_temp_dirs(child_pid);
    fail_closed(code, detail)
}

const HOLDER_OK: u8 = 1;
/// Sent instead of [`HOLDER_OK`] when the user and network namespaces are
/// there but the PID namespace is not.
const HOLDER_NO_PID_NAMESPACE: u8 = 2;

/// Length of the message that releases the holder: the proxy port, then the
/// IDE port, both little-endian and 0 for none.
const HOLDER_RELEASE_LENGTH: usize = 4;

fn encode_holder_release(proxy_port: u16, ide_port: u16) -> [u8; HOLDER_RELEASE_LENGTH] {
    let [proxy_low, proxy_high] = proxy_port.to_le_bytes();
    let [ide_low, ide_high] = ide_port.to_le_bytes();
    [proxy_low, proxy_high, ide_low, ide_high]
}

fn decode_holder_release(release: [u8; HOLDER_RELEASE_LENGTH]) -> (u16, u16) {
    let [proxy_low, proxy_high, ide_low, ide_high] = release;
    (
        u16::from_le_bytes([proxy_low, proxy_high]),
        u16::from_le_bytes([ide_low, ide_high]),
    )
}

/// The forked holder. Runs after the parent has mapped its ids and stood up the
/// network. It still holds full capabilities inside its own user namespace (it
/// never `exec`s a helper stage that would drop them), so it can build the mount
/// jail directly before finally `exec`ing the untrusted command. Never returns.
fn run_holder(
    setup: HolderSetup<'_>,
    ready_write_fd: RawFd,
    go_read_fd: RawFd,
    relay_socket_fd: RawFd,
) -> ! {
    match run_holder_inner(setup, ready_write_fd, go_read_fd, relay_socket_fd) {
        Ok(never) => match never {},
        Err((code, error)) => fail_closed(code, &format!("{error:#}")),
    }
}

/// What the holder inherits from the parent across the `fork`.
struct HolderSetup<'a> {
    invocation: Stage1Invocation,
    fs_backend: FsBackend,
    writable_pins: &'a [CanonicalPathBuf],
    /// ADR-F49 decision 14: what the command may read among the hidden entries,
    /// pinned as the roots are, so a cover binds back the very object that was
    /// checked.
    readable_pins: &'a [CanonicalPathBuf],
    protected: &'a [ProtectedEntry],
    plan: &'a FilesystemPlan,
    /// The soft open-file limit to hand back to the command (see
    /// [`raise_open_file_limit`]).
    original_open_file_limit: Option<libc::rlimit>,
    /// The signal mask to hand back to the command: the parent blocked the
    /// signals it takes over across the fork (see [`block_command_signals`]).
    original_signal_mask: libc::sigset_t,
    /// The parent's pid, for the holder to notice a parent that died before
    /// its parent-death signal was set.
    parent_pid: libc::pid_t,
    /// The descriptor of the controlling terminal to hand to the command's
    /// process group, when the helper's group had it in the foreground (see
    /// [`foreground_terminal`]).
    terminal: Option<RawFd>,
    /// ADR-F49 decision 11: the Linux home's entries the command may not read.
    /// On WSL init finds the Windows homes' itself.
    read_hidden: &'a [PathBuf],
    /// What stays reachable however it lies among them (see
    /// [`reachable_paths`]).
    reachable: &'a [PathBuf],
    /// Directories covered whole around the roots inside them (see
    /// [`cover_enclosure`]).
    enclosures: &'a [PathBuf],
}

fn run_holder_inner(
    setup: HolderSetup<'_>,
    ready_write_fd: RawFd,
    go_read_fd: RawFd,
    relay_socket_fd: RawFd,
) -> std::result::Result<std::convert::Infallible, (&'static str, anyhow::Error)> {
    // A holder that outlived the parent would keep the namespace, and with it
    // the command, running with nobody left to clean up after it.
    die_with_parent(setup.parent_pid).map_err(|error| ("signals-failed", error))?;
    enter_command_namespaces(ready_write_fd)?;
    // Report readiness, then block until the parent maps our ids and hands us the
    // ports to listen on. A short read means the parent aborted.
    write_all_bytes(ready_write_fd, &[HOLDER_OK])
        .map_err(|error| ("handshake-failed", anyhow::Error::new(error)))?;
    let mut release = [0u8; HOLDER_RELEASE_LENGTH];
    read_exact_bytes(go_read_fd, &mut release).map_err(|error| {
        (
            "handshake-failed",
            anyhow!("parent aborted before release: {error}"),
        )
    })?;
    let (proxy_port, ide_port) = decode_holder_release(release);

    // Publish the listeners *inside* the namespace. slirp has no host-loopback
    // forwarding and no working NAT, so nothing here can reach the host on its
    // own; the parent (which is outside) accepts on these sockets and splices
    // them to the proxy and to the IDE bridge. Created before the FS jail and
    // the seccomp filter, both of which the command must not be able to work
    // around. The IDE one first: its port is fixed, and the egress one then
    // takes any port that is left.
    if ide_port != 0 {
        publish_listener(relay_socket_fd, ide_port).map_err(|error| ("relay-failed", error))?;
    }
    let relay_port = if proxy_port != 0 {
        Some(publish_listener(relay_socket_fd, 0).map_err(|error| ("relay-failed", error))?)
    } else {
        None
    };
    if ide_port != 0 || proxy_port != 0 {
        // Then wait for the parent to finish taking the namespace's network
        // apart again (see `stop_slirp` there): until that byte arrives, slirp
        // is still running and its NAT can still reach addresses the host owns.
        let mut released = [0u8; 1];
        read_exact_bytes(go_read_fd, &mut released).map_err(|error| {
            (
                "handshake-failed",
                anyhow!("parent aborted before the network was final: {error}"),
            )
        })?;
    }

    launch_namespace_init(setup, relay_port)
}

/// Have the kernel SIGKILL this process when its parent dies, and exit at once
/// when the parent is gone already — it may have died before the request, and
/// then no signal comes.
fn die_with_parent(parent_pid: libc::pid_t) -> Result<()> {
    set_parent_death_signal()?;
    // SAFETY: `getppid` takes no arguments and cannot fail.
    if unsafe { libc::getppid() } != parent_pid {
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    }
    Ok(())
}

fn set_parent_death_signal() -> Result<()> {
    // SAFETY: `prctl` with scalar arguments only.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        bail!(
            "prctl(PR_SET_PDEATHSIG): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Create the command's user, network and PID namespaces (unprivileged NEWNET
/// and NEWPID are only allowed together with NEWUSER). As the creator the
/// holder holds every capability inside the new user namespace, and so does
/// the init it forks.
///
/// The PID namespace is what keeps the command — same uid as the helper, and
/// on WSL's 6.6 kernel without Landlock's ABI-6 signal scoping — from
/// signalling the helper parent, the IDE bridge or another session's helper,
/// and it lets the kernel stop everything the command leaves behind when the
/// namespace's init exits. `unshare(CLONE_NEWPID)` only arms it: the next fork
/// becomes the namespace's init. There is no running without it.
fn enter_command_namespaces(
    ready_write_fd: RawFd,
) -> std::result::Result<(), (&'static str, anyhow::Error)> {
    // SAFETY: `unshare` with these flags reads no memory.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET | libc::CLONE_NEWPID) } == 0
    {
        return Ok(());
    }
    let pid_namespace_error = std::io::Error::last_os_error();
    // `unshare` is all or nothing, so nothing was entered above. A host without
    // user namespaces at all needs different advice than one refusing only the
    // PID namespace.
    // SAFETY: as above.
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
        // Returning here drops the holder's `ready` write end when the process
        // exits via `fail_closed`, so the parent's read sees EOF and doesn't
        // hang.
        return Err((
            "userns-failed",
            anyhow!(
                "unshare(CLONE_NEWUSER|CLONE_NEWNET): {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    log::warn!("[remote-sandbox] unshare(CLONE_NEWPID): {pid_namespace_error}");
    // The parent prints the marker line for this one: the client reads only the
    // first, and a line from here would come before the parent's.
    if let Err(error) = write_all_bytes(ready_write_fd, &[HOLDER_NO_PID_NAMESPACE]) {
        log::warn!("[remote-sandbox] could not report the missing PID namespace: {error}");
    }
    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
}

/// Apply the filesystem backend to what the parent pinned and planned,
/// returning the directory the command's `TMPDIR` should name. `scratch_pid`
/// names the per-command scratch directories, as the parent removes them.
#[allow(
    clippy::too_many_arguments,
    reason = "the backend applies every part of the policy the parent worked out"
)]
fn apply_filesystem_backend(
    fs_backend: FsBackend,
    invocation: &Stage1Invocation,
    writable_pins: &[CanonicalPathBuf],
    readable_pins: &[CanonicalPathBuf],
    protected: &[ProtectedEntry],
    plan: &FilesystemPlan,
    scratch_pid: libc::pid_t,
    read_hidden: &[PathBuf],
    reachable: &[PathBuf],
    enclosures: &[PathBuf],
) -> std::result::Result<PathBuf, (&'static str, anyhow::Error)> {
    match fs_backend {
        FsBackend::Landlock(abi) => {
            let mut temporary_writable = Vec::new();
            let tmpdir = if invocation.wsl {
                enter_private_mount_namespace().map_err(|error| ("mount-failed", error))?;
                // First, so that the read-only binds of protected entries in a
                // worktree on a Windows drive land on what this binds back.
                cover_windows_drives(writable_pins, invocation, &mut Vec::new()).map_err(
                    |error| {
                        let code = if error.is::<WindowsHomeRoot>() {
                            "windows-home-root"
                        } else {
                            "mount-failed"
                        };
                        (code, error)
                    },
                )?;
                let tmpdir = provide_private_tmp(
                    writable_pins,
                    plan,
                    invocation.allow_fs_write,
                    &mut temporary_writable,
                    scratch_pid,
                )
                .map_err(|error| ("mount-failed", error))?;
                provide_private_dev_shm(writable_pins, &mut temporary_writable);
                for pin in &plan.binds {
                    bind_read_only(pin).map_err(|error| ("mount-failed", error))?;
                }
                hide_read_hidden_entries(
                    read_hidden,
                    reachable,
                    enclosures,
                    writable_pins,
                    readable_pins,
                    None,
                    false,
                )
                .or_else(|error| read_hiding_refused(invocation.wsl, error))?;
                tmpdir
            } else {
                let private_tmp = create_private_tmpdir(scratch_pid)
                    .map_err(|error| ("mkdtemp-failed", error))?;
                temporary_writable.push(private_tmp.clone());
                // Nothing else here needs a mount namespace of the command's
                // own, the hiding does: entered only when there is something
                // to hide.
                hide_read_hidden_entries(
                    read_hidden,
                    reachable,
                    enclosures,
                    writable_pins,
                    readable_pins,
                    invocation.on_wsl.then_some(invocation),
                    true,
                )
                .or_else(|error| read_hiding_refused(invocation.wsl, error))?;
                private_tmp
            };
            apply_landlock(abi, &plan.grants, &temporary_writable, invocation.on_wsl)
                .map_err(|error| ("landlock-failed", error))?;
            Ok(tmpdir)
        }
        FsBackend::MountJail => {
            // The jail can only re-bind what exists, so a missing protected
            // entry stays creatable on these kernels (5.4, no Landlock).
            let protected_pins: Vec<CanonicalPathBuf> = protected
                .iter()
                .filter_map(|entry| entry.pin().cloned())
                .collect();
            apply_mount_jail(
                writable_pins,
                &protected_pins,
                invocation.allow_fs_write,
                scratch_pid,
            )
            .map_err(|error| ("mount-failed", error))?;
            // Inside the jail's own mount namespace, after `pivot_root`.
            hide_read_hidden_entries(
                read_hidden,
                reachable,
                enclosures,
                writable_pins,
                readable_pins,
                invocation.on_wsl.then_some(invocation),
                false,
            )
            .or_else(|error| read_hiding_refused(invocation.wsl, error))?;
            Ok(PathBuf::from("/tmp"))
        }
    }
}

/// What a command's `PATH` is when the client names none: the default a login
/// gets on any Linux, and nothing else.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The command's whole environment, built from nothing (ADR-F46 C-1).
///
/// Whatever started the helper has an environment of its own — an SSH session's
/// `SSH_AUTH_SOCK` and `SSH_CONNECTION`, whatever the remote login shell
/// exported on the way in, `wsl.exe`'s interop variables — and until now the
/// command inherited all of it. An agent running for hours inside the sandbox
/// with the user's forwarded agent socket in its environment is exactly what
/// the sandbox exists to prevent, so the command gets only:
///
/// * what the client asked for (`--setenv`), which is the project's own
///   settings and the agent's, resolved outside the sandbox;
/// * a default `PATH`, so a command still finds the system's own programs;
/// * `HOME` / `USER` / `LOGNAME` from the account database — never from the
///   environment, which is the session's — so that `git`, `npm` and `cargo`
///   find their configuration and caches. The client on a remote host does not
///   know where the account's home is, so it cannot send these itself;
/// * the locale (`LANG`, `LC_*`) and, for an interactive session, `TERM`:
///   those describe the terminal the client is showing, not the session's
///   identity, and without them a command's output is wrong;
/// * the sandbox's own settings — its private temp directory and its proxy —
///   which always win over the rest.
fn command_environment(
    invocation: &Stage1Invocation,
    tmpdir: &Path,
    relay_port: Option<u16>,
) -> Vec<(OsString, OsString)> {
    let mut environment: Vec<(OsString, OsString)> = Vec::new();
    let set = |environment: &mut Vec<(OsString, OsString)>, name: &OsStr, value: OsString| {
        match environment
            .iter_mut()
            .find(|(existing, _)| existing == name)
        {
            Some((_, existing)) => *existing = value,
            None => environment.push((name.to_os_string(), value)),
        }
    };
    set(
        &mut environment,
        OsStr::new("PATH"),
        OsString::from(DEFAULT_PATH),
    );
    // Before the client's `--setenv`, so that a value the client sends
    // explicitly (a test, a `HOME` an agent wants elsewhere) wins over the
    // account database. Read from the account this process acts as, not from
    // its environment: on WSL that is the Windows side's, and on any host a
    // `HOME` inherited from the session is the very thing the sandbox drops.
    if let Some(account) = account_from_passwd() {
        set(
            &mut environment,
            OsStr::new("HOME"),
            account.home.into_os_string(),
        );
        // `pw_name` can be empty; leave `USER` / `LOGNAME` unset rather than
        // set them to nothing.
        if !account.name.is_empty() {
            set(&mut environment, OsStr::new("USER"), account.name.clone());
            set(&mut environment, OsStr::new("LOGNAME"), account.name);
        }
    }
    for (name, value) in std::env::vars_os() {
        let Some(text) = name.to_str() else {
            continue;
        };
        let describes_the_terminal =
            text == "LANG" || text.starts_with("LC_") || (invocation.interactive && text == "TERM");
        if describes_the_terminal {
            set(&mut environment, &name, value);
        }
    }
    for (name, value) in &invocation.set_environment {
        set(&mut environment, name, value.clone());
    }
    for name in &invocation.unset_environment {
        environment.retain(|(existing, _)| existing != name);
    }
    // How a process inside WSL finds the Windows interop socket. The seccomp
    // filter already makes that socket unusable; this stops a command from even
    // trying, whichever transport started the helper and whatever the client
    // asked to set.
    if invocation.on_wsl {
        for name in ["WSL_INTEROP", "WSLENV"] {
            environment.retain(|(existing, _)| existing != OsStr::new(name));
        }
    }
    for name in ["TMPDIR", "TMP", "TEMP"] {
        set(
            &mut environment,
            OsStr::new(name),
            tmpdir.as_os_str().to_os_string(),
        );
    }
    // The in-namespace relay listener: the sandbox's own loopback, not the
    // host's.
    if let Some(relay_port) = relay_port {
        let url = OsString::from(format!("http://127.0.0.1:{relay_port}"));
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            set(&mut environment, OsStr::new(name), url.clone());
        }
        for name in ["NO_PROXY", "no_proxy"] {
            set(&mut environment, OsStr::new(name), OsString::new());
        }
    }
    environment
}

/// Drop capabilities, install the seccomp filter, restore the signal mask and
/// `exec` the command. The filter survives `exec`; the command reaches the
/// proxy over TCP (AF_INET), which the filter allows. Only returns on failure.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              `CommandExt::exec` of the untrusted command is exactly what it must do"
)]
fn finalize_and_exec_command(
    invocation: &Stage1Invocation,
    original_signal_mask: &libc::sigset_t,
    environment: &[(OsString, OsString)],
) -> (&'static str, anyhow::Error) {
    // WSL runs commands as root by default, as can a dev container's
    // `remoteUser`, and root in this user namespace would keep every capability
    // across `exec` — `CAP_SYS_ADMIN` over the mount namespace holding the
    // read-only binds or the mount jail included (Bubblewrap drops them the
    // same way). Anyone else loses them at `exec` anyway.
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if invocation.on_wsl || unsafe { libc::geteuid() } == 0 {
        if let Err(error) = drop_all_capabilities() {
            return ("caps-failed", error);
        }
    }
    if let Err(error) = install_command_seccomp_filter() {
        return ("seccomp-failed", error);
    }
    // A handler init or the holder installed is inherited as `SIG_IGN` across
    // the fork, and `exec` keeps an ignored disposition — so a command would
    // otherwise never see SIGINT. Put the signals we touch back to their
    // default, then unblock (the parent blocked them across the fork so it
    // could decide whom to hand them to).
    reset_touched_signals_to_default();
    restore_signal_mask(original_signal_mask);
    // `env_clear` first: the command's environment is the one built for it, not
    // this process's (see [`command_environment`]). `PATH` is among what is set,
    // so looking the program up by name uses the command's own `PATH`.
    let error = Command::new(&invocation.program)
        .args(&invocation.args)
        .env_clear()
        .envs(environment.iter().map(|(name, value)| (name, value)))
        .exec();
    (
        "exec-failed",
        anyhow!("exec {:?}: {error}", invocation.program),
    )
}

/// Fork the command's PID-namespace init and wait for it. Init sets up the
/// filesystem, the environment and the terminal and runs the command; the
/// holder only passes signals on to it and exits with its status. When init
/// exits the kernel kills whatever else is left in the namespace. Only returns
/// when the fork fails.
fn launch_namespace_init(
    setup: HolderSetup<'_>,
    relay_port: Option<u16>,
) -> std::result::Result<std::convert::Infallible, (&'static str, anyhow::Error)> {
    // The scratch directories carry the holder's pid, which is what the parent
    // knows to remove afterwards; init's own pid, inside the namespace, is 1.
    // SAFETY: `getpid` takes no arguments and cannot fail.
    let scratch_pid = unsafe { libc::getpid() };
    // `getppid` cannot tell init whether the holder is still there — the
    // holder is outside init's namespace, so the answer is always 0 — but this
    // pipe's write end closing can.
    let holder_alive = Pipe::new().map_err(|error| ("pipe-failed", anyhow::Error::new(error)))?;
    // SAFETY: a single `fork` from a single-threaded process (no thread has
    // been started in the holder). The child is the new namespace's init.
    let init = unsafe { libc::fork() };
    if init < 0 {
        return Err((
            "fork-failed",
            anyhow!(
                "fork() for the PID-namespace init: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    if init == 0 {
        drop(holder_alive.write);
        run_namespace_init(setup, relay_port, scratch_pid, holder_alive.read);
    }
    drop(holder_alive.read);
    if let Err(error) = install_command_signal_policy(init) {
        log::warn!("[remote-sandbox] could not install the holder signal policy: {error:#}");
    }
    restore_signal_mask(&setup.original_signal_mask);
    // Init's death by a signal is the namespace being torn down (PDEATHSIG, an
    // OOM kill), not the command dying — otherwise init would exit with the
    // command's code. Note it so a `128 + signal` status is not read as the
    // command having caught that signal.
    let mut raw_status = 0;
    // SAFETY: `raw_status` is a valid out-parameter; `init` is our direct child.
    let waited = unsafe { libc::waitpid(init, &mut raw_status, 0) };
    if waited < 0 {
        fail_closed("wait-failed", "failed to wait for the sandbox init");
    }
    if libc::WIFSIGNALED(raw_status) {
        eprintln!(
            "[remote-sandbox] the sandbox init was killed by signal {}, not the command",
            libc::WTERMSIG(raw_status)
        );
    }
    let code = if libc::WIFEXITED(raw_status) {
        libc::WEXITSTATUS(raw_status)
    } else if libc::WIFSIGNALED(raw_status) {
        128 + libc::WTERMSIG(raw_status)
    } else {
        SANDBOX_SETUP_FAILED_EXIT_CODE
    };
    std::process::exit(code);
}

/// PID 1 of the command's namespace. Never returns.
fn run_namespace_init(
    setup: HolderSetup<'_>,
    relay_port: Option<u16>,
    scratch_pid: libc::pid_t,
    holder_alive: OwnedFd,
) -> ! {
    match prepare_and_run_command(setup, relay_port, scratch_pid, holder_alive) {
        Ok(never) => match never {},
        Err((code, error)) => fail_closed(code, &format!("{error:#}")),
    }
}

/// Init's work: die with the holder, mount the namespace's own `/proc` —
/// before the filesystem backend, because Landlock refuses every mount once it
/// is enforced — apply the backend, set up the command's environment, then fork
/// the command into a process group of its own, hand it the terminal, and reap
/// until it exits.
fn prepare_and_run_command(
    setup: HolderSetup<'_>,
    relay_port: Option<u16>,
    scratch_pid: libc::pid_t,
    holder_alive: OwnedFd,
) -> std::result::Result<std::convert::Infallible, (&'static str, anyhow::Error)> {
    let HolderSetup {
        invocation,
        fs_backend,
        writable_pins,
        readable_pins,
        protected,
        plan,
        original_open_file_limit,
        original_signal_mask,
        parent_pid: _,
        terminal,
        read_hidden,
        reachable,
        enclosures,
    } = setup;
    set_parent_death_signal().map_err(|error| ("signals-failed", error))?;
    // Checked after the request, so a holder that died before it is seen here
    // and one that dies after it sends the signal.
    if pipe_writers_are_gone(&holder_alive) {
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    }
    drop(holder_alive);

    mount_private_proc();
    let tmpdir = apply_filesystem_backend(
        fs_backend,
        &invocation,
        writable_pins,
        readable_pins,
        protected,
        plan,
        scratch_pid,
        read_hidden,
        reachable,
        enclosures,
    )?;
    let environment = command_environment(&invocation, &tmpdir, relay_port);
    // After the FS jail is in place, so the path resolves inside the jail.
    if let Some(dir) = &invocation.chdir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("chdir into {}", dir.display()))
            .map_err(|error| ("chdir-failed", error))?;
    }
    restore_open_file_limit(original_open_file_limit);

    // SAFETY: single-threaded (init has started no thread); the child becomes
    // the command.
    let command = unsafe { libc::fork() };
    if command < 0 {
        return Err((
            "fork-failed",
            anyhow!(
                "fork() for the sandboxed command: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    if command == 0 {
        let (code, error) =
            become_command(&invocation, &original_signal_mask, terminal, &environment);
        fail_closed(code, &format!("{error:#}"));
    }
    // Done on both sides of the fork, whichever gets there first, as a shell
    // does for a job: the command may read the terminal the moment it runs.
    move_to_own_process_group(command, terminal);
    if let Err(error) = install_command_signal_policy(command) {
        log::warn!("[remote-sandbox] could not install the init signal policy: {error:#}");
    }
    restore_signal_mask(&original_signal_mask);
    // Reaps the command's orphans, which the kernel hands to init, and keeps a
    // stopped command from hanging. Once init exits the kernel kills everything
    // else in the namespace.
    let code = wait_for_command(command).unwrap_or(SANDBOX_SETUP_FAILED_EXIT_CODE);
    std::process::exit(code);
}

/// Whether every write end of the pipe `read_end` belongs to has closed.
fn pipe_writers_are_gone(read_end: &OwnedFd) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: read_end.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`; a zero timeout never blocks.
    let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    ready > 0 && poll_fd.revents & libc::POLLHUP != 0
}

/// The command's side of the fork: a process group of its own and, when the
/// helper had it, the terminal; then `exec`. Only returns on failure.
fn become_command(
    invocation: &Stage1Invocation,
    original_signal_mask: &libc::sigset_t,
    terminal: Option<RawFd>,
    environment: &[(OsString, OsString)],
) -> (&'static str, anyhow::Error) {
    // `kill(0, …)` reaches the caller's whole process group, and a process
    // group is not confined to a PID namespace: left in the helper's group, the
    // command could stop or kill the parent, the holder and init with it.
    // SAFETY: `setpgid` with scalar arguments.
    if unsafe { libc::setpgid(0, 0) } != 0 {
        return (
            "signals-failed",
            anyhow!("setpgid: {}", std::io::Error::last_os_error()),
        );
    }
    if let Some(terminal) = terminal {
        // SAFETY: `getpid` takes no arguments and cannot fail.
        hand_terminal_to(terminal, unsafe { libc::getpid() });
    }
    finalize_and_exec_command(invocation, original_signal_mask, environment)
}

/// Init's half of putting the command in a process group of its own and
/// handing it the terminal, so that Ctrl+C, Ctrl+Z and window-size changes
/// reach the command's group and nothing else.
fn move_to_own_process_group(command: libc::pid_t, terminal: Option<RawFd>) {
    // SAFETY: `setpgid` with scalar arguments. Once the command has `exec`ed
    // this fails with `EACCES`, and by then it has done it itself.
    let made_group = unsafe { libc::setpgid(command, command) } == 0;
    if !made_group {
        log::debug!(
            "[remote-sandbox] setpgid for the command: {}",
            std::io::Error::last_os_error()
        );
    }
    if let Some(terminal) = terminal {
        // SAFETY: `tcgetpgrp` with a scalar argument.
        let foreground = unsafe { libc::tcgetpgrp(terminal) };
        // Inside this PID namespace a group outside it has no id, so 0 is the
        // helper's group: the terminal still waiting to be handed over. The
        // command's own group (the one set up above) has it already, and any
        // other group is a job the command started, which keeps it. Right
        // after `setpgid` succeeded the command has not run, so there is no
        // such job yet.
        let hand_over = foreground >= 0 && foreground != command && (made_group || foreground == 0);
        if hand_over {
            hand_terminal_to(terminal, command);
        }
    }
}

/// Make `group` the terminal's foreground process group. Called from outside
/// that group, which the terminal allows only while SIGTTOU is blocked or
/// ignored: blocked since before the fork in init and the command, ignored by
/// the parent when it takes the terminal back.
fn hand_terminal_to(terminal: RawFd, group: libc::pid_t) {
    // SAFETY: `tcsetpgrp` with scalar arguments.
    if unsafe { libc::tcsetpgrp(terminal, group) } != 0 {
        log::warn!(
            "[remote-sandbox] could not hand the terminal to process group {group}: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Take the terminal back for the helper's own `group` once the command is
/// done, but only if its foreground group is gone (the command's, now dead) —
/// never steal it from a live job that legitimately became the foreground.
fn reclaim_terminal(terminal: RawFd, group: libc::pid_t) {
    // SAFETY: `tcgetpgrp` with a scalar argument.
    let foreground = unsafe { libc::tcgetpgrp(terminal) };
    if foreground < 0 || foreground == group {
        return;
    }
    // The group, not a process with the group's id: the leader may be gone
    // while the rest of its job still runs.
    // SAFETY: `kill` with signal 0 only probes; `EPERM` still means it is alive.
    let alive = unsafe { libc::kill(-foreground, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        hand_terminal_to(terminal, group);
    }
}

/// The descriptor among stdin, stdout and stderr that is this process's
/// controlling terminal with this process's group in the foreground, and that
/// group: the terminal the command is to get, and whom to give it back to. A
/// helper in a background job, or without a terminal, hands nothing over.
fn foreground_terminal() -> Option<(RawFd, libc::pid_t)> {
    // SAFETY: `getpgrp` takes no arguments and cannot fail.
    let own_group = unsafe { libc::getpgrp() };
    [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
        .into_iter()
        // SAFETY: `tcgetpgrp` with a scalar argument; -1 for anything that is
        // not this process's controlling terminal.
        .find(|&descriptor| unsafe { libc::tcgetpgrp(descriptor) } == own_group)
        .map(|descriptor| (descriptor, own_group))
}

/// Mount a private `/proc` reflecting the current PID namespace. Done in a
/// fresh mount namespace so it never touches the host's `/proc`, and left best
/// effort (the command runs regardless).
fn mount_private_proc() {
    // SAFETY: `unshare` with this flag reads no memory.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        log::warn!(
            "[remote-sandbox] no private mount namespace for /proc: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    if let Err(error) = mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    ) {
        log::warn!("[remote-sandbox] could not make / private for a fresh /proc: {error:#}");
        return;
    }
    if let Err(error) = mount(
        Some(Path::new("proc")),
        Path::new("/proc"),
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    ) {
        log::warn!("[remote-sandbox] could not mount a fresh /proc: {error:#}");
    }
}

/// Set the signals the helper parent, the holder and init touch back to their
/// default disposition, so the command starts with normal behavior (`exec`
/// would otherwise keep an inherited `SIG_IGN`).
///
/// SIGTTIN and SIGTTOU are deliberately *left ignored*: if the command is not
/// the terminal's foreground group (a helper launched in the background, or one
/// with a console it is not the foreground of — the common non-interactive
/// `wsl.exe` case), a background terminal read or write would otherwise stop
/// the command with nobody in the sandbox to continue it, hanging init. Ignored,
/// those operations return `EIO` instead of stopping — non-blocking, and a
/// sandboxed command has no business reading the user's terminal from the
/// background anyway. When the command *is* the foreground group (an interactive
/// pty session), these never fire regardless.
fn reset_touched_signals_to_default() {
    let reset = FORWARDED_SIGNALS
        .into_iter()
        .chain(IGNORED_SIGNALS)
        .filter(|signal| !matches!(*signal, libc::SIGTTIN | libc::SIGTTOU));
    for signal in reset {
        if let Err(error) = set_signal_disposition(signal, libc::SIG_DFL, 0) {
            log::debug!("[remote-sandbox] could not reset signal {signal}: {error:#}");
        }
    }
}

/// Read exactly `buf.len()` bytes from `fd`, failing on an early EOF.
fn read_exact_bytes(fd: RawFd, buf: &mut [u8]) -> std::io::Result<()> {
    let mut read = 0;
    while read < buf.len() {
        // SAFETY: `fd` is a valid readable descriptor; the slice is in bounds.
        let ret = unsafe { libc::read(fd, buf[read..].as_mut_ptr().cast(), buf.len() - read) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if ret == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pipe closed",
            ));
        }
        read += ret as usize;
    }
    Ok(())
}

/// A `pipe2(O_CLOEXEC)` pair. CLOEXEC is correct: the holder uses the fds before
/// it `exec`s the command (which then closes them), keeping them out of the
/// untrusted command.
struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> std::io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a valid 2-element array.
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both fds are freshly created and owned by us.
        unsafe {
            Ok(Self {
                read: OwnedFd::from_raw_fd(fds[0]),
                write: OwnedFd::from_raw_fd(fds[1]),
            })
        }
    }
}

/// A `socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC)`, the only kind of
/// channel an fd can travel over. CLOEXEC for the same reason as [`Pipe`]: both
/// ends are used before the holder `exec`s the untrusted command.
struct SocketPair {
    parent: OwnedFd,
    holder: OwnedFd,
}

impl SocketPair {
    fn new() -> std::io::Result<Self> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a valid 2-element array.
        let ret = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both fds are freshly created and owned by us.
        unsafe {
            Ok(Self {
                parent: OwnedFd::from_raw_fd(fds[0]),
                holder: OwnedFd::from_raw_fd(fds[1]),
            })
        }
    }
}

/// Bind one of the sandbox's ways out — the egress entry point (`port` 0: any
/// free port) or the IDE port — on the namespace's own loopback, and hand the
/// listening socket to the parent, which is the only participant that can
/// still reach the host. Returns the port it listens on.
///
/// Binding here — inside the namespace, before any restriction is applied — is
/// what makes the tighter slirp configuration possible: the parent cannot
/// create a socket in a namespace it is not in, and joining one would mean
/// entering the holder's user namespace and losing the ability to come back
/// out.
fn publish_listener(socket_fd: RawFd, port: u16) -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("binding the in-sandbox listener on port {port}"))?;
    let port = listener
        .local_addr()
        .context("reading the in-sandbox listener address")?
        .port();
    send_relay_listener(socket_fd, listener.as_raw_fd(), port)?;
    // The parent's copy is what serves the sandbox from now on; ours would only
    // be one more descriptor to keep away from the command.
    drop(listener);
    Ok(port)
}

/// Send the listening socket (plus its port as the payload) over `SCM_RIGHTS`.
fn send_relay_listener(socket_fd: RawFd, listener_fd: RawFd, port: u16) -> Result<()> {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    let payload = port.to_le_bytes();
    let iov = [std::io::IoSlice::new(&payload)];
    let fds = [listener_fd];
    let control = [ControlMessage::ScmRights(&fds)];
    sendmsg::<()>(socket_fd, &iov, &control, MsgFlags::MSG_NOSIGNAL, None)
        .map_err(std::io::Error::from)
        .context("handing the in-sandbox egress listener to the helper parent")?;
    Ok(())
}

/// Receive the listening socket the holder created inside the namespace. It
/// arrives with `O_CLOEXEC` (`MSG_CMSG_CLOEXEC`) so it cannot leak into any
/// process we spawn later.
fn receive_relay_listener(socket_fd: RawFd) -> Result<OwnedFd> {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    let mut payload = [0u8; 2];
    let mut iov = [std::io::IoSliceMut::new(&mut payload)];
    let mut control = nix::cmsg_space!([RawFd; 1]);
    let message = recvmsg::<()>(
        socket_fd,
        &mut iov,
        Some(&mut control),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(std::io::Error::from)
    .context("receiving the in-sandbox egress listener")?;
    if message.flags.contains(MsgFlags::MSG_CTRUNC) {
        bail!("the in-sandbox egress listener was truncated in transit");
    }

    let mut received = Vec::new();
    for control_message in message.cmsgs().map_err(std::io::Error::from)? {
        if let ControlMessageOwned::ScmRights(raw_fds) = control_message {
            for raw in raw_fds {
                // SAFETY: the kernel installed each `SCM_RIGHTS` descriptor as a
                // fresh, open fd in our table for this message, so it is valid
                // and aliases nothing we already hold; `nix` does not take
                // ownership, so wrapping each exactly once makes us its owner.
                received.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
    }
    // Extra descriptors (only a broken holder would send any) are closed by
    // dropping the rest of the vector.
    let listener = received.into_iter().next();
    listener.context("the holder sent no in-sandbox egress listener")
}

/// Accept on the sandbox-side egress listener and splice each connection to
/// the egress proxy on the host's loopback. This is the whole of the sandbox's
/// outbound path: it can reach the proxy's allowlist decision and nothing else.
fn spawn_egress_relay(listener: OwnedFd, proxy_port: u16) -> std::io::Result<()> {
    spawn_relay(
        listener,
        "egress",
        MAX_EGRESS_RELAY_CONNECTIONS,
        move |inbound| match std::net::TcpStream::connect((
            std::net::Ipv4Addr::LOCALHOST,
            proxy_port,
        )) {
            Ok(outbound) => splice_connection(inbound, outbound, "egress"),
            Err(error) => {
                log::warn!("[remote-sandbox] egress relay could not reach the proxy: {error}")
            }
        },
    )
}

/// ADR-F49: accept on the sandbox-side IDE listener and splice each connection
/// to the IDE bridge's socket.
fn spawn_ide_relay(listener: OwnedFd, rendezvous: IdeRendezvous) -> std::io::Result<()> {
    let rendezvous = Arc::new(rendezvous);
    spawn_relay(
        listener,
        "ide",
        MAX_IDE_RELAY_CONNECTIONS,
        move |inbound| match rendezvous.connect() {
            Ok(outbound) => splice_connection(inbound, outbound, "ide"),
            Err(error) => {
                log::warn!("[remote-sandbox] ide relay could not reach the bridge: {error}")
            }
        },
    )
}

/// The most IDE connections the helper relays at once, as many as the bridge
/// and Zed take.
const MAX_IDE_RELAY_CONNECTIONS: usize = crate::MAX_BRIDGE_CONNECTIONS;
/// The most egress connections the helper relays at once. Higher than the IDE
/// cap because parallel downloads legitimately open many; it only bounds the
/// threads (two per connection) a command can make the helper start.
const MAX_EGRESS_RELAY_CONNECTIONS: usize = 256;

/// Accept on `listener` and hand each connection to `serve` on a thread of its
/// own, never more than `max_connections` at once: one beyond that is closed
/// as soon as it is accepted.
fn spawn_relay(
    listener: OwnedFd,
    relay: &'static str,
    max_connections: usize,
    serve: impl Fn(std::net::TcpStream) + Clone + Send + 'static,
) -> std::io::Result<()> {
    let listener = std::net::TcpListener::from(listener);
    let open_connections = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new()
        .name(format!("zed-remote-sandbox-{relay}"))
        .spawn(move || {
            loop {
                let inbound = match listener.accept() {
                    Ok((inbound, _peer)) => inbound,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        log::warn!("[remote-sandbox] {relay} relay stopped accepting: {error}");
                        break;
                    }
                };
                // Only this thread adds, so the count cannot pass the cap
                // between the check and the increment.
                if open_connections.load(Ordering::SeqCst) >= max_connections {
                    log::warn!(
                        "[remote-sandbox] {relay} relay refused a connection: \
                         {max_connections} are open"
                    );
                    drop(inbound);
                    continue;
                }
                open_connections.fetch_add(1, Ordering::SeqCst);
                let slot = RelaySlot(open_connections.clone());
                let serve = serve.clone();
                // A failed spawn drops the closure, and with it the slot.
                if let Err(error) = std::thread::Builder::new()
                    .name(format!("zed-remote-sandbox-{relay}-conn"))
                    .spawn(move || {
                        let _slot = slot;
                        serve(inbound)
                    })
                {
                    log::warn!(
                        "[remote-sandbox] {relay} relay could not take a connection: {error}"
                    );
                }
            }
        })?;
    Ok(())
}

/// One of a relay's open connections; frees its place when dropped.
struct RelaySlot(Arc<AtomicUsize>);

impl Drop for RelaySlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connected stream the relays splice. Each direction is copied on a thread
/// of its own, so the stream has to be duplicable and half-closable.
trait RelayStream: std::io::Read + std::io::Write + Send + Sized + 'static {
    fn duplicate(&self) -> std::io::Result<Self>;
    fn shutdown_write(&self) -> std::io::Result<()>;
}

impl RelayStream for std::net::TcpStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_write(&self) -> std::io::Result<()> {
        self.shutdown(std::net::Shutdown::Write)
    }
}

impl RelayStream for UnixStream {
    fn duplicate(&self) -> std::io::Result<Self> {
        self.try_clone()
    }

    fn shutdown_write(&self) -> std::io::Result<()> {
        self.shutdown(std::net::Shutdown::Write)
    }
}

/// Pump one accepted sandbox connection to and from `outbound` until both
/// directions end.
fn splice_connection(inbound: impl RelayStream, outbound: impl RelayStream, relay: &'static str) {
    let (mut inbound_reader, mut outbound_writer) =
        match (inbound.duplicate(), outbound.duplicate()) {
            (Ok(inbound_reader), Ok(outbound_writer)) => (inbound_reader, outbound_writer),
            _ => {
                log::warn!("[remote-sandbox] {relay} relay could not duplicate a relayed socket");
                return;
            }
        };

    let forward = std::thread::Builder::new()
        .name(format!("zed-remote-sandbox-{relay}-out"))
        .spawn(move || {
            // A relayed connection normally ends with a reset or a half-close
            // from one side, so a copy error here is expected traffic, not a
            // fault.
            if let Err(error) = std::io::copy(&mut inbound_reader, &mut outbound_writer) {
                log::debug!("[remote-sandbox] {relay} relay request stream ended: {error}");
            }
            if let Err(error) = outbound_writer.shutdown_write() {
                log::debug!("[remote-sandbox] {relay} relay could not half-close: {error}");
            }
        });
    let forward = match forward {
        Ok(forward) => forward,
        Err(error) => {
            log::warn!("[remote-sandbox] {relay} relay could not start: {error}");
            return;
        }
    };

    let mut outbound_reader = outbound;
    let mut inbound_writer = inbound;
    if let Err(error) = std::io::copy(&mut outbound_reader, &mut inbound_writer) {
        log::debug!("[remote-sandbox] {relay} relay response stream ended: {error}");
    }
    if let Err(error) = inbound_writer.shutdown_write() {
        log::debug!("[remote-sandbox] {relay} relay could not half-close: {error}");
    }
    if forward.join().is_err() {
        log::warn!("[remote-sandbox] {relay} relay request thread panicked");
    }
}

/// Take over the listeners the holder created inside the network namespace —
/// the IDE one first, then the egress one, in the order it sends them — and
/// start serving them. Returns whether they all arrived: one that did not
/// means the holder failed closed on its own. Failing to serve one that did
/// arrive fails the command closed.
fn take_over_namespace_listeners(
    relay_socket_fd: RawFd,
    ide_rendezvous: Option<IdeRendezvous>,
    egress_proxy_port: Option<u16>,
    child_pid: libc::pid_t,
) -> bool {
    if let Some(rendezvous) = ide_rendezvous {
        match receive_relay_listener(relay_socket_fd) {
            Ok(listener) => {
                if let Err(error) = spawn_ide_relay(listener, rendezvous) {
                    abort_holder(
                        child_pid,
                        "relay-failed",
                        &format!("starting the IDE relay: {error}"),
                    );
                }
            }
            Err(error) => {
                log::warn!("[remote-sandbox] the holder sent no IDE listener: {error:#}");
                return false;
            }
        }
    }
    if let Some(proxy_port) = egress_proxy_port {
        match receive_relay_listener(relay_socket_fd) {
            Ok(listener) => {
                if let Err(error) = spawn_egress_relay(listener, proxy_port) {
                    abort_holder(
                        child_pid,
                        "relay-failed",
                        &format!("starting the egress relay: {error}"),
                    );
                }
            }
            Err(error) => {
                log::warn!("[remote-sandbox] the holder sent no egress listener: {error:#}");
                return false;
            }
        }
    }
    true
}

/// The IDE bridge's socket as the helper's parent connects to it (ADR-F49).
///
/// The parent connects from outside every namespace, so a socket the command
/// could put there — or a symlink to one, WSL's interop socket say — would be
/// reached with the parent's reach. Only a directory inside the protected set
/// will do, which the command cannot write to however its writable roots lie,
/// and the socket is looked at, without following a symlink, before every
/// connection.
struct IdeRendezvous {
    directory: OwnedFd,
    socket_name: CString,
}

impl IdeRendezvous {
    fn open(
        path: &Path,
        protected: &[ProtectedEntry],
        writable_pins: &[CanonicalPathBuf],
    ) -> Result<Self> {
        let socket_name = match path.file_name() {
            Some(name) => CString::new(name.as_bytes())
                .with_context(|| format!("{} contains a NUL byte", path.display()))?,
            None => bail!("{} does not name a socket", path.display()),
        };
        let parent = path
            .parent()
            .with_context(|| format!("{} has no directory", path.display()))?;
        let directory_path = std::fs::canonicalize(parent)
            .with_context(|| format!("resolving {}", parent.display()))?;
        ensure!(
            protected
                .iter()
                .filter_map(ProtectedEntry::pin)
                .any(|pin| directory_path.starts_with(pin.path())),
            "{} is not inside a protected directory, so the sandboxed command could \
             replace the socket the helper connects to",
            directory_path.display()
        );
        // A protected directory is never also a writable grant (the two may not
        // overlap), but check anyway: the parent connects from outside every
        // namespace, so a rendezvous the command can write to is a way to aim
        // that connection.
        ensure!(
            !writable_pins
                .iter()
                .any(|pin| directory_path.starts_with(pin.path())),
            "{} is inside a writable grant, so the sandboxed command could replace \
             the socket the helper connects to",
            directory_path.display()
        );
        let directory = crate::util::open_o_path(&directory_path, crate::util::FollowSymlinks::No)
            .with_context(|| format!("opening {}", directory_path.display()))?;
        let status = nix::sys::stat::fstat(directory.as_raw_fd())
            .map_err(|errno| anyhow!("examining {}: {errno}", directory_path.display()))?;
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let own_uid = unsafe { libc::geteuid() };
        ensure!(
            status.st_mode & libc::S_IFMT == libc::S_IFDIR
                && status.st_uid == own_uid
                && status.st_mode & 0o077 == 0,
            "{} is not a private directory of this account",
            directory_path.display()
        );
        Ok(Self {
            directory,
            socket_name,
        })
    }

    /// Connect to the bridge without a path re-resolution the command could
    /// race: open the socket itself relative to the pinned directory with
    /// `O_PATH | O_NOFOLLOW` (so a symlink planted at the name fails, and the
    /// directory is not walked again), confirm through that very descriptor
    /// that it is a socket of this account's, then `connect` to
    /// `/proc/self/fd/<that descriptor>` — the same inode, not a name resolved
    /// a second time.
    fn connect(&self) -> std::io::Result<UnixStream> {
        // SAFETY: `directory` is open and `socket_name` is NUL-terminated; an
        // `O_PATH` open takes no mode and performs no I/O on the socket.
        let raw = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                self.socket_name.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: freshly opened by us and owned by nothing else.
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let status = nix::sys::stat::fstat(socket.as_raw_fd()).map_err(std::io::Error::from)?;
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let own_uid = unsafe { libc::geteuid() };
        if status.st_mode & libc::S_IFMT != libc::S_IFSOCK || status.st_uid != own_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the IDE rendezvous is not a socket of this account",
            ));
        }
        UnixStream::connect(format!("/proc/self/fd/{}", socket.as_raw_fd()))
    }
}

/// The command, which SIGHUP / SIGTERM are passed on to; 0 while there is none
/// — before the fork, and once it has been reaped and its pid may be someone
/// else's.
static COMMAND_PID: AtomicI32 = AtomicI32::new(0);

/// Passed on to the command: a hangup (its terminal went away) or a request to
/// terminate is meant for it, and the parent has to outlive it to clean up.
const FORWARDED_SIGNALS: [libc::c_int; 2] = [libc::SIGHUP, libc::SIGTERM];
/// Ignored by the parent, the holder and init. The terminal sends the first
/// three to its foreground group, which is the command's own once init has
/// handed it over; sent to the helper by anything else, they are not meant for
/// the sandbox's plumbing either. SIGTTOU / SIGTTIN would stop them for writing
/// to the terminal from the background, which they are once the command has it.
const IGNORED_SIGNALS: [libc::c_int; 5] = [
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTOU,
    libc::SIGTTIN,
];

fn become_child_subreaper() -> Result<()> {
    // SAFETY: `prctl` with scalar arguments only.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        bail!(
            "prctl(PR_SET_CHILD_SUBREAPER): {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Block the signals the parent takes over, returning the mask to restore.
fn block_command_signals() -> Result<libc::sigset_t> {
    // SAFETY: both sets are initialized (by `sigemptyset` and by
    // `pthread_sigmask`) before anything reads them.
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        for signal in FORWARDED_SIGNALS.into_iter().chain(IGNORED_SIGNALS) {
            libc::sigaddset(&mut blocked, signal);
        }
        let mut original: libc::sigset_t = std::mem::zeroed();
        let result = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut original);
        if result != 0 {
            bail!(
                "pthread_sigmask: {}",
                std::io::Error::from_raw_os_error(result)
            );
        }
        Ok(original)
    }
}

fn restore_signal_mask(mask: &libc::sigset_t) {
    // SAFETY: `mask` is a signal set `pthread_sigmask` filled in.
    let result = unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, mask, std::ptr::null_mut()) };
    if result != 0 {
        log::warn!(
            "[remote-sandbox] could not restore the signal mask: {}",
            std::io::Error::from_raw_os_error(result)
        );
    }
}

/// Pass SIGHUP / SIGTERM on to `command`, and ignore SIGINT / SIGQUIT /
/// SIGTSTP.
fn install_command_signal_policy(command: libc::pid_t) -> Result<()> {
    COMMAND_PID.store(command, Ordering::SeqCst);
    for signal in FORWARDED_SIGNALS {
        set_signal_disposition(
            signal,
            forward_to_command as extern "C" fn(libc::c_int) as libc::sighandler_t,
            libc::SA_RESTART,
        )?;
    }
    for signal in IGNORED_SIGNALS {
        set_signal_disposition(signal, libc::SIG_IGN, 0)?;
    }
    Ok(())
}

fn set_signal_disposition(
    signal: libc::c_int,
    handler: libc::sighandler_t,
    flags: libc::c_int,
) -> Result<()> {
    // SAFETY: an all-zero `sigaction` is a valid value; it is completed below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    action.sa_flags = flags;
    // SAFETY: `action.sa_mask` is a valid signal set to empty, and the handler
    // is either `SIG_IGN` or `forward_to_command`, which is async-signal-safe.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            bail!("sigaction({signal}): {}", std::io::Error::last_os_error());
        }
    }
    Ok(())
}

extern "C" fn forward_to_command(signal: libc::c_int) {
    let command = COMMAND_PID.load(Ordering::SeqCst);
    if command > 0 {
        // SAFETY: `kill` is async-signal-safe, and `errno` is put back so the
        // code this handler interrupted never sees ours. Should the command be
        // gone already, there is nobody left to tell.
        unsafe {
            let errno = libc::__errno_location();
            let saved = *errno;
            libc::kill(command, signal);
            *errno = saved;
        }
    }
}

/// Wait for the command, reaping on the way the orphans it leaves behind — so
/// none lingers as a zombie while it runs (the subreaper inherits them on the
/// stage-1 parent; the namespace init is handed them as PID 1). A command that
/// stops (Ctrl+Z sends SIGTSTP to the terminal's foreground group) is continued
/// at once, since nothing here would ever continue it: Ctrl+Z does nothing.
/// The command is observed before it is reaped, so its pid stays its own until
/// nothing can pass a signal on to it any more.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              blocking on its children is correct here"
)]
fn wait_for_command(command: libc::pid_t) -> Result<i32> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value for `waitid` to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // `WNOWAIT` leaves the state change in place: a stop can be answered
        // with SIGCONT without consuming it, and an exit is reaped explicitly
        // below so the code is read from `wait_pid`.
        // SAFETY: `info` is a valid out-parameter.
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WSTOPPED | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            bail!("waitid: {error}");
        }
        // SAFETY: `waitid` reported a child; the union carries its pid.
        let reported = unsafe { info.si_pid() };
        if info.si_code == libc::CLD_STOPPED {
            // SAFETY: a `CLD_STOPPED` report carries the stop signal.
            let stop_signal = unsafe { info.si_status() };
            if reported == command && !matches!(stop_signal, libc::SIGTTIN | libc::SIGTTOU) {
                // Continue the command's whole process group (its pid is the
                // group id), so a stopped pipeline — `sh -c 'cat | grep x'`, an
                // agent CLI's tool subprocess — resumes as a whole, not just its
                // leader. SIGTTIN/SIGTTOU stops are left alone: continuing one
                // would only loop into the same stop.
                // SAFETY: `kill` with scalar arguments; the command is our
                // unreaped child, so its pid — and its group — is still its own.
                if unsafe { libc::kill(-command, libc::SIGCONT) } != 0 {
                    log::debug!(
                        "[remote-sandbox] continuing the command's group: {}",
                        std::io::Error::last_os_error()
                    );
                }
            } else {
                // `WNOWAIT` left the stop in place, and the next wait would
                // report it again at once: consumed here instead. A stopped
                // orphan stays stopped until the namespace goes. A command
                // stopped for using the terminal from the background waits, as
                // under any shell, rather than being woken into the same stop
                // over and over.
                let mut status = 0;
                // SAFETY: `status` is a valid out-parameter; `reported` is our
                // unreaped child.
                if unsafe { libc::waitpid(reported, &mut status, libc::WUNTRACED | libc::WNOHANG) }
                    < 0
                {
                    log::debug!(
                        "[remote-sandbox] consuming the stop of {reported}: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            continue;
        }
        if reported == command {
            COMMAND_PID.store(0, Ordering::SeqCst);
            return wait_pid(command);
        }
        if let Err(error) = wait_pid(reported) {
            log::debug!("[remote-sandbox] reaping an orphan of the command: {error:#}");
        }
    }
}

/// Stop everything the command left running once it has exited — daemons that
/// `setsid` away included. As the subreaper we inherit each orphan when its
/// parent dies, so killing our children over and over reaches the whole tree.
/// A child's pid stays ours until we reap it, so none can have been reused by
/// the time we signal it.
fn terminate_remaining_descendants() {
    const GIVE_UP_AFTER: Duration = Duration::from_secs(5);
    let started = Instant::now();
    loop {
        reap_exited_children();
        let children = living_children();
        if children.is_empty() {
            return;
        }
        if started.elapsed() > GIVE_UP_AFTER {
            log::warn!(
                "[remote-sandbox] {} processes the command left behind did not stop",
                children.len()
            );
            return;
        }
        for child in children {
            // SAFETY: `kill` with scalar arguments; `child` is our unreaped
            // child, so the pid is still its own.
            if unsafe { libc::kill(child, libc::SIGKILL) } != 0 {
                log::debug!(
                    "[remote-sandbox] stopping {child}: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// How long a command has to stop once its client went away, before the
/// sandbox is taken down around it.
const HANGUP_GRACE: Duration = Duration::from_secs(5);
/// How long the parent waits, once the command and everything it left are gone,
/// for what they wrote to reach the client.
const DRAIN_GRACE: Duration = Duration::from_secs(5);
/// Copied between the client and the command in one go.
const RELAY_BUFFER_SIZE: usize = 64 * 1024;

/// Whether the client has already been found gone, so that whichever notices
/// first does the stopping and the other leaves it alone.
static CLIENT_GONE: AtomicBool = AtomicBool::new(false);

/// ADR-F49 decision 9: stop the command once nobody can read its output any
/// more — `ssh`, `wsl.exe` or Zed itself gone, or the terminal closed. The
/// command gets SIGHUP, as from a terminal that went away, and after
/// [`HANGUP_GRACE`] the holder is killed, which takes the command's PID
/// namespace down with it. The command's stdin closing is left to the command:
/// an ACP agent ends its session on it, a build keeps building.
fn client_went_away(reason: &str) {
    if CLIENT_GONE.swap(true, Ordering::SeqCst) {
        return;
    }
    log::info!("[remote-sandbox] the client went away ({reason}); stopping the command");
    forward_to_command(libc::SIGHUP);
    // The grace period waits in a thread of its own, so whoever noticed goes on
    // with its own work: the relay keeps draining what the command writes, and
    // a command blocked on a full pipe can act on the SIGHUP instead of sitting
    // there until it is killed.
    let kill = std::thread::Builder::new()
        .name("zed-remote-sandbox-hangup".to_string())
        .spawn(|| {
            std::thread::sleep(HANGUP_GRACE);
            forward_to_command(libc::SIGKILL);
        });
    if let Err(error) = kill {
        log::warn!("[remote-sandbox] no thread for the hangup grace period: {error}");
        std::thread::sleep(HANGUP_GRACE);
        forward_to_command(libc::SIGKILL);
    }
}

/// Watch the client's own end of the command's output for the client going
/// away, for a command that writes nothing (the relay below notices when it
/// writes). A pipe reports its reader gone (`POLLERR`), a terminal or a socket
/// its other side (`POLLHUP`).
fn watch_for_client_hangup(streams: StandardStreams) -> std::io::Result<()> {
    if !streams.output_was_open {
        // Nothing was there to watch when the helper started, and what
        // `ensure_standard_descriptors` put in its place is ours.
        return Ok(());
    }
    std::thread::Builder::new()
        .name("zed-remote-sandbox-watch".to_string())
        .spawn(|| {
            if output_hung_up(libc::STDOUT_FILENO) {
                client_went_away("its end of the command's output is closed");
            }
        })
        .map(drop)
}

/// Blocks until the other end of `fd` is gone and returns `true`; returns
/// `false` at once when `fd` is not open. One that can never report it — a
/// file, `/dev/null` — keeps it waiting for good.
fn output_hung_up(fd: RawFd) -> bool {
    loop {
        let mut poll_fd = libc::pollfd {
            fd,
            events: 0,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` for the duration of the call. No events are
        // asked for, so only those always reported — `POLLERR`, `POLLHUP`,
        // `POLLNVAL` — end the wait.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // Watching is over, and the command keeps running: say so, rather
            // than leaving a sandbox nobody is watching look like one that is.
            log::warn!(
                "[remote-sandbox] no longer watching for the client's hangup: poll: {error}"
            );
            return false;
        }
        if poll_fd.revents & libc::POLLNVAL != 0 {
            log::warn!(
                "[remote-sandbox] no longer watching for the client's hangup: stdout closed"
            );
            return false;
        }
        if poll_fd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return true;
        }
    }
}

/// What the helper's standard streams were when it started (see
/// [`ensure_standard_descriptors`]).
#[derive(Clone, Copy)]
struct StandardStreams {
    /// Whether the client's end of the command's input is a pipe or a socket —
    /// what a client that spawned the helper with redirected streams gives it,
    /// and what the relay is for.
    input_is_a_pipe: bool,
    output_is_a_pipe: bool,
    /// Whether the command's output was open at all when the helper started.
    output_was_open: bool,
}

/// Make sure the three standard descriptors are open, opening `/dev/null` for
/// one that is not, and report what they are.
///
/// A helper started with, say, its output closed would otherwise hand
/// descriptor 1 to the first pipe it creates — and the hangup watch would then
/// take that pipe's own end closing for the client going away and kill a
/// perfectly healthy command.
fn ensure_standard_descriptors() -> Result<StandardStreams> {
    let mut streams = StandardStreams {
        input_is_a_pipe: false,
        output_is_a_pipe: false,
        output_was_open: true,
    };
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        match nix::sys::stat::fstat(fd) {
            Ok(stat) => {
                let kind = stat.st_mode & libc::S_IFMT;
                let is_a_pipe = kind == libc::S_IFIFO || kind == libc::S_IFSOCK;
                if fd == libc::STDIN_FILENO {
                    streams.input_is_a_pipe = is_a_pipe;
                }
                if fd == libc::STDOUT_FILENO {
                    streams.output_is_a_pipe = is_a_pipe;
                }
            }
            Err(nix::errno::Errno::EBADF) => {
                if fd == libc::STDOUT_FILENO {
                    streams.output_was_open = false;
                }
                open_dev_null_at(fd)?;
            }
            Err(errno) => bail!("fstat of descriptor {fd}: {errno}"),
        }
    }
    Ok(streams)
}

/// Open `/dev/null` as descriptor `fd`, which has to be closed for this to be
/// called at all.
fn open_dev_null_at(fd: RawFd) -> Result<()> {
    // SAFETY: a NUL-terminated literal path and scalar flags; `open` takes no
    // mode without `O_CREAT`.
    let opened = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if opened < 0 {
        bail!(
            "opening /dev/null for descriptor {fd}: {}",
            std::io::Error::last_os_error()
        );
    }
    if opened == fd {
        // `O_CLOEXEC` would close it again at the command's `exec`, and the
        // point is that the descriptor stays taken.
        clear_close_on_exec(fd)?;
        return Ok(());
    }
    // SAFETY: both are open descriptors of ours; `dup2` leaves the copy without
    // `O_CLOEXEC`.
    let duplicated = unsafe { libc::dup2(opened, fd) };
    let error = std::io::Error::last_os_error();
    // SAFETY: `opened` is ours and is not needed any more.
    unsafe { libc::close(opened) };
    if duplicated < 0 {
        bail!("placing /dev/null at descriptor {fd}: {error}");
    }
    Ok(())
}

fn clear_close_on_exec(fd: RawFd) -> Result<()> {
    // SAFETY: `fcntl` with scalar arguments on a descriptor of ours.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
        bail!(
            "keeping descriptor {fd} open across exec: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// ADR-F49 decision 9: the parent's own pipes between the client's standard
/// streams and the command's.
///
/// The command never gets the client's own descriptors. Handed the write end of
/// the client's pipe, it can open that pipe for *reading* through
/// `/proc/self/fd/1`: the pipe then has a reader whatever the client does, no
/// `POLLERR` ever comes, and a command that ignores the end of its input
/// outlives the session that started it — the one thing the hangup watch
/// exists to prevent. Relayed, the client's end is the parent's alone, and what
/// the command holds is a pipe of ours whose readers it may add as it likes.
///
/// Only for streams that are pipes or sockets, and never for an interactive
/// session: there the streams are the terminal, which the command must have as
/// its own (its controlling terminal, its window size, its job control), and a
/// terminal reports its hangup to everyone holding it anyway.
struct StdioRelay {
    /// Client → command.
    input: Option<Pipe>,
    /// Command → client.
    output: Option<Pipe>,
}

impl StdioRelay {
    fn new(invocation: &Stage1Invocation, streams: StandardStreams) -> Result<Self> {
        if invocation.interactive || !streams.output_is_a_pipe {
            return Ok(Self {
                input: None,
                output: None,
            });
        }
        Ok(Self {
            input: streams.input_is_a_pipe.then(Pipe::new).transpose()?,
            output: Some(Pipe::new()?),
        })
    }

    /// In the holder, right after the fork: put the command's ends of the
    /// relay at descriptors 0 and 1 and close everything else of it, so that
    /// nothing inside the sandbox holds the client's own descriptors.
    fn enter_command_side(self) -> Result<()> {
        if let Some(input) = &self.input {
            place_descriptor(input.read.as_raw_fd(), libc::STDIN_FILENO)?;
        }
        if let Some(output) = &self.output {
            place_descriptor(output.write.as_raw_fd(), libc::STDOUT_FILENO)?;
        }
        drop(self);
        Ok(())
    }

    /// In the parent: keep the ends the relay threads use and close the
    /// command's.
    fn into_parent_side(self) -> ParentRelay {
        ParentRelay {
            input: self.input.map(|pipe| pipe.write),
            output: self.output.map(|pipe| pipe.read),
        }
    }
}

/// The parent's ends of [`StdioRelay`].
struct ParentRelay {
    input: Option<OwnedFd>,
    output: Option<OwnedFd>,
}

impl ParentRelay {
    /// Start relaying. The threads run with the command's signals still
    /// blocked, as the other parent threads do.
    fn start(self) -> std::io::Result<RunningRelay> {
        if let Some(input) = self.input {
            std::thread::Builder::new()
                .name("zed-remote-sandbox-input".to_string())
                .spawn(move || {
                    match relay_stream(libc::STDIN_FILENO, input.as_raw_fd()) {
                        // Dropping `input` closes the command's stdin, which
                        // reaches it as the end of its input — exactly what the
                        // client's own pipe would have done.
                        RelayOutcome::SourceEnded | RelayOutcome::TargetGone => {}
                        RelayOutcome::Failed(error) => {
                            log::warn!("[remote-sandbox] relaying the client's input: {error}");
                        }
                    }
                    drop(input);
                })?;
        }
        let Some(output) = self.output else {
            return Ok(RunningRelay { drained: None });
        };
        let (drained, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("zed-remote-sandbox-output".to_string())
            .spawn(move || {
                match relay_stream(output.as_raw_fd(), libc::STDOUT_FILENO) {
                    RelayOutcome::SourceEnded => {}
                    RelayOutcome::TargetGone => {
                        client_went_away("its end of the command's output is closed");
                        // Keep reading and dropping what is still written, so a
                        // command blocked on a full pipe can act on the SIGHUP.
                        discard_stream(output.as_raw_fd());
                    }
                    RelayOutcome::Failed(error) => {
                        log::warn!("[remote-sandbox] relaying the command's output: {error}");
                    }
                }
                // The command and everything it left have closed their end, so
                // the client has everything they wrote.
                if drained.try_send(()).is_err() {
                    log::debug!("[remote-sandbox] nobody was waiting for the output to drain");
                }
            })?;
        Ok(RunningRelay {
            drained: Some(receiver),
        })
    }
}

/// A relay that is running (see [`ParentRelay::start`]).
struct RunningRelay {
    drained: Option<std::sync::mpsc::Receiver<()>>,
}

impl RunningRelay {
    /// Wait for what the command wrote to reach the client, once nothing of the
    /// command is left to write more. Without this the parent could exit — and
    /// take the relay with it — with the command's last answer still in the
    /// pipe.
    fn wait_until_drained(&self) {
        let Some(drained) = &self.drained else {
            return;
        };
        if let Err(error) = drained.recv_timeout(DRAIN_GRACE) {
            log::warn!(
                "[remote-sandbox] the command's output did not drain before the helper exited: \
                 {error}"
            );
        }
    }
}

/// Why [`relay_stream`] stopped.
enum RelayOutcome {
    /// Nothing more will be written to it: every writer closed.
    SourceEnded,
    /// Nobody is reading what is relayed any more.
    TargetGone,
    Failed(std::io::Error),
}

/// Copy everything from `from` to `to` until one end gives out.
fn relay_stream(from: RawFd, to: RawFd) -> RelayOutcome {
    let mut buffer = vec![0u8; RELAY_BUFFER_SIZE];
    loop {
        // SAFETY: `buffer` is valid for `buffer.len()` bytes for the call.
        let read = unsafe { libc::read(from, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return RelayOutcome::Failed(error);
        }
        if read == 0 {
            return RelayOutcome::SourceEnded;
        }
        let written = buffer.get(..read as usize).unwrap_or_default();
        if let Err(error) = write_all_bytes(to, written) {
            return if error.kind() == std::io::ErrorKind::BrokenPipe {
                RelayOutcome::TargetGone
            } else {
                RelayOutcome::Failed(error)
            };
        }
    }
}

/// Read `fd` to its end and drop what it carries.
fn discard_stream(fd: RawFd) {
    let mut buffer = vec![0u8; RELAY_BUFFER_SIZE];
    loop {
        // SAFETY: as in `relay_stream`.
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if read <= 0 {
            return;
        }
    }
}

/// Put `descriptor` at `target`, leaving it open across `exec`.
fn place_descriptor(descriptor: RawFd, target: RawFd) -> Result<()> {
    // SAFETY: both are open descriptors of ours; `dup2` closes whatever `target`
    // was and leaves the copy without `O_CLOEXEC`.
    if unsafe { libc::dup2(descriptor, target) } < 0 {
        bail!(
            "placing descriptor {descriptor} at {target}: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn reap_exited_children() {
    loop {
        let mut status = 0;
        // SAFETY: `status` is a valid out-parameter.
        let reaped = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        // 0: none has exited; -1: none is left (or the wait failed, which
        // leaves nothing to do either).
        if reaped <= 0 {
            return;
        }
    }
}

/// Our children that are still running, from `/proc` (a zombie is done).
fn living_children() -> Vec<libc::pid_t> {
    let own_pid = std::process::id();
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("[remote-sandbox] cannot list processes: {error}");
            return Vec::new();
        }
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let (state, parent) = parse_process_state_and_parent(&stat)?;
            (u32::try_from(parent).ok() == Some(own_pid) && !matches!(state, 'Z' | 'X'))
                .then_some(pid)
        })
        .collect()
}

/// The state and the parent pid from a `/proc/<pid>/stat` line.
fn parse_process_state_and_parent(stat: &str) -> Option<(char, libc::pid_t)> {
    // The command name comes in parentheses and may itself hold spaces and
    // parentheses, so the fields are read after the last `)`.
    let after_command = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = after_command.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let parent = fields.next()?.parse().ok()?;
    Some((state, parent))
}

fn read_one_byte(fd: RawFd) -> std::io::Result<u8> {
    let mut byte = [0u8; 1];
    // SAFETY: `fd` is a valid readable descriptor; `byte` is a 1-byte buffer.
    let ret = unsafe { libc::read(fd, byte.as_mut_ptr().cast(), 1) };
    if ret == 1 {
        Ok(byte[0])
    } else if ret == 0 {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "pipe closed",
        ))
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn write_all_bytes(fd: RawFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        // SAFETY: `fd` is a valid writable descriptor; the slice is in bounds.
        let ret =
            unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error());
        }
        written += ret as usize;
    }
    Ok(())
}

/// `waitpid` the holder and reduce its status to an exit code (128+signal for a
/// signal death), mirroring the shell convention.
fn wait_pid(pid: libc::pid_t) -> Result<i32> {
    let mut status = 0;
    // SAFETY: `status` is a valid int pointer; `pid` is our direct child.
    let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
    if ret < 0 {
        return Err(anyhow!("waitpid failed"));
    }
    if libc::WIFEXITED(status) {
        Ok(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Ok(128 + libc::WTERMSIG(status))
    } else {
        Ok(SANDBOX_SETUP_FAILED_EXIT_CODE)
    }
}

/// Write the holder's `setgroups=deny`, `gid_map`, and `uid_map`, mapping our
/// real ids identically (`<id> <id> 1`).
///
/// An unprivileged parent may write exactly one line mapping its own effective
/// id, which is what this does — so it works on 24.04 where the holder's own
/// self-write is refused. Keeping the real uid is what lets the command write
/// worktree files it owns.
fn write_id_maps(pid: libc::pid_t) -> Result<()> {
    // SAFETY: these getters are always successful and take no arguments.
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    std::fs::write(format!("/proc/{pid}/setgroups"), "deny")
        .with_context(|| format!("writing /proc/{pid}/setgroups"))?;
    std::fs::write(format!("/proc/{pid}/gid_map"), format!("{gid} {gid} 1"))
        .with_context(|| format!("writing /proc/{pid}/gid_map"))?;
    std::fs::write(format!("/proc/{pid}/uid_map"), format!("{uid} {uid} 1"))
        .with_context(|| format!("writing /proc/{pid}/uid_map"))?;
    Ok(())
}

/// Locate the bundled `slirp4netns` next to the remote server binary (its
/// `~/.zed_server` directory, or the WSL backend's install directory). Only
/// there: it runs outside the sandbox, and `PATH` comes from whoever started
/// this process.
fn locate_slirp4netns(self_exe: &Path) -> Option<PathBuf> {
    let candidate = self_exe.parent()?.join(SLIRP_BINARY_NAME);
    candidate.is_file().then_some(candidate)
}

/// The `slirp4netns` to run, and when the Windows backend named the digest it
/// shipped, the descriptor of the file that was checked against it.
struct SlirpExecutable {
    path: PathBuf,
    verified: Option<OwnedFd>,
}

impl SlirpExecutable {
    /// What to `exec`: the checked descriptor itself when there is one, so the
    /// file cannot be swapped between the check and the start.
    fn exec_path(&self) -> PathBuf {
        match &self.verified {
            Some(fd) => PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd())),
            None => self.path.clone(),
        }
    }
}

fn verify_slirp(path: PathBuf, expected_sha256: Option<&str>) -> Result<SlirpExecutable> {
    use sha2::{Digest as _, Sha256};
    let Some(expected_sha256) = expected_sha256 else {
        return Ok(SlirpExecutable {
            path,
            verified: None,
        });
    };
    let mut file =
        std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    let actual: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        bail!(
            "{} does not match the {SLIRP_BINARY_NAME} this build shipped",
            path.display()
        );
    }
    // Moved above fd 3, which `start_slirp` overwrites with the readiness pipe
    // just before `exec`.
    // SAFETY: `F_DUPFD_CLOEXEC` only reads the open descriptor and returns a new
    // one, which is owned here from then on.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if fd < 0 {
        bail!(
            "duplicating the {SLIRP_BINARY_NAME} descriptor: {}",
            std::io::Error::last_os_error()
        );
    }
    // SAFETY: freshly returned by `fcntl`, owned solely by us.
    let verified = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(SlirpExecutable {
        path,
        verified: Some(verified),
    })
}

/// Stop `slirp4netns` and reap it. Only diagnostics can come of a failure: the
/// namespace it served is being torn down either way.
fn stop_slirp(slirp: &mut std::process::Child) {
    if let Err(error) = slirp.kill() {
        log::debug!("[remote-sandbox] stopping slirp4netns: {error}");
    }
    if let Err(error) = slirp.wait() {
        log::debug!("[remote-sandbox] reaping slirp4netns: {error}");
    }
}

/// Start `slirp4netns` attached to the holder's network namespace, and block
/// until it signals (via `--ready-fd`) that the interface is configured.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              blocking process APIs are correct here"
)]
fn start_slirp(slirp: &SlirpExecutable, holder_pid: libc::pid_t) -> Result<std::process::Child> {
    let ready = Pipe::new().context("creating slirp ready pipe")?;
    let ready_write_raw = ready.write.as_raw_fd();
    let ready_read_raw = ready.read.as_raw_fd();

    let mut command = Command::new(slirp.exec_path());
    command
        .arg("--configure")
        // A large MTU keeps throughput reasonable over the tap device.
        .arg("--mtu=65520")
        // The command reaches DNS through the proxy (which resolves host-side),
        // so slirp's own resolver is unnecessary and left off.
        .arg("--disable-dns")
        // Bind slirp's outbound socket to loopback: external NAT then fails
        // ("Network is unreachable").
        .arg("--outbound-addr=127.0.0.1")
        // ...and take away the one destination that survived that: libslirp's
        // built-in `10.0.2.2` alias forwards to the *host's* `127.0.0.1`, which
        // reaches every local service (sshd, a local model server, a database)
        // and — because a socket bound to 127.0.0.1 may still reach any address
        // the host itself owns — the host's own LAN address too. Egress now
        // exists only through the relayed listener in `publish_relay_listener`.
        .arg("--disable-host-loopback")
        .arg("--ready-fd=3")
        .arg(holder_pid.to_string())
        .arg("tap0")
        .stdin(std::process::Stdio::null());
    // slirp4netns reports its progress on stdout and stderr, which here are the
    // command's own: keep that out of the output the user and the model read,
    // unless the helper's diagnostics were asked for (see `init_helper_logging`),
    // and even then off stdout, which an ACP agent's client reads as JSON-RPC.
    if std::env::var_os("ZED_REMOTE_SANDBOX_LOG").is_none() {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    } else {
        use std::os::fd::AsFd as _;
        match std::io::stderr().as_fd().try_clone_to_owned() {
            Ok(stderr) => {
                command.stdout(std::process::Stdio::from(stderr));
            }
            Err(error) => {
                log::warn!("[remote-sandbox] slirp4netns's reports go nowhere: {error}");
                command.stdout(std::process::Stdio::null());
            }
        }
    }

    // SAFETY: the closure runs post-fork/pre-exec in slirp's child. `dup2` is
    // async-signal-safe and the only thing it does; the duplicated fd (3) loses
    // CLOEXEC so slirp inherits it as its ready-fd.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(ready_write_raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().context("spawning slirp4netns")?;
    // Close our copy of the write end so EOF is observable if slirp dies.
    drop(ready.write);
    // Block until slirp writes its readiness byte; a closed pipe means it gave
    // up before configuring the namespace.
    let ready_result = read_one_byte(ready_read_raw);
    drop(ready.read);
    if let Err(error) = ready_result {
        stop_slirp(&mut child);
        bail!("slirp4netns did not configure the namespace: {error}");
    }
    Ok(child)
}

/// A command's own decision under an any-host grant when no editor-wide
/// allowlist was sent — always on an SSH host, which is never sent one: every
/// host, but only at a public address. Local addresses are the host's own
/// services (its sshd, a local model server, a database), its LAN and on a
/// cloud host the metadata service; from WSL also the Windows host behind the
/// NAT gateway, and with mirrored networking Windows' loopback. With an
/// allowlist in force it decides about those instead, exactly as the
/// in-process proxy's ceiling does elsewhere, however the helper was started.
/// The addresses the host itself holds are local too, whatever their range: the
/// proxy runs in the host's own network namespace, so a `CONNECT` to the host's
/// global IPv6 address or its public IPv4 one reaches whatever is bound to
/// `0.0.0.0` or `::` there — its sshd, a database, a local model server. The
/// rebinding filter cannot know them (they are perfectly ordinary public
/// addresses), so they are read from the interfaces at the moment of the
/// connection: an address added or given up since the helper started is
/// accounted for, and a name that resolves to one is refused as well.
struct PublicAddressesOnly;

impl http_proxy::DestinationPolicy for PublicAddressesOnly {
    fn evaluate(&self, _host: &str, _port: u16) -> http_proxy::PolicyDecision {
        http_proxy::PolicyDecision::Allow {
            permit_local_addresses: false,
        }
    }

    fn forbids_address(&self, address: std::net::IpAddr) -> bool {
        is_an_address_of_this_host(address)
    }
}

/// Whether `address` is one this host holds, comparing what
/// `getifaddrs` reports interface by interface. An IPv4 address an IPv6 one
/// stands for is compared as IPv4 (a dual-stack socket reaches the host's own
/// IPv4 service through `::ffff:a.b.c.d`, and NAT64, 6to4 or an IPv4-compatible
/// route reaches it through the other embedded forms), and a failure to read
/// the interfaces refuses the address: not knowing is not a reason to connect.
fn is_an_address_of_this_host(address: std::net::IpAddr) -> bool {
    use std::net::IpAddr;

    // The same folding `is_forbidden_ip` uses, so an embedded-IPv4 encoding of
    // this host's own public address is caught, not only the IPv4-mapped one.
    let comparable = |address: IpAddr| match address {
        IpAddr::V6(v6) => http_proxy::embedded_ipv4(v6).map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    };
    let wanted = comparable(address);
    let interfaces = match nix::ifaddrs::getifaddrs() {
        Ok(interfaces) => interfaces,
        Err(errno) => {
            log::warn!(
                "[remote-sandbox] refusing {address}: this host's own addresses cannot be read \
                 ({errno})"
            );
            return true;
        }
    };
    for interface in interfaces {
        let Some(storage) = interface.address else {
            continue;
        };
        let own = match (storage.as_sockaddr_in(), storage.as_sockaddr_in6()) {
            (Some(v4), _) => IpAddr::V4(v4.ip()),
            (_, Some(v6)) => IpAddr::V6(v6.ip()),
            _ => continue,
        };
        if comparable(own) == wanted {
            return true;
        }
    }
    false
}

/// What replaces the allowlist's own decision in the egress proxy:
/// [`PublicAddressesOnly`] for an any-host grant that no editor-wide allowlist
/// caps, because `Allowlist::any()` on its own waives the proxy's filter on
/// local addresses.
fn any_host_policy(
    network: &RemoteSandboxNetwork,
    network_ceiling: Option<&Arc<crate::EditorAllowlistCeiling>>,
) -> Option<Arc<dyn http_proxy::DestinationPolicy>> {
    if network_ceiling.is_none() && matches!(network, RemoteSandboxNetwork::AnyHost) {
        Some(Arc::new(PublicAddressesOnly))
    } else {
        None
    }
}

/// Stand up the host-loopback egress proxy enforcing the per-command allowlist,
/// nested inside the editor-wide one when the WSL backend sent it.
fn start_egress_proxy(
    network: &RemoteSandboxNetwork,
    network_ceiling: Option<&Arc<crate::EditorAllowlistCeiling>>,
) -> Result<http_proxy::ProxyHandle> {
    use http_proxy::{Allowlist, DestinationPolicy, HostPattern, ProxyConfig, ProxyHandle};

    let policy = any_host_policy(network, network_ceiling);
    let allowlist = match network {
        RemoteSandboxNetwork::Blocked => Allowlist::from_patterns(Vec::new()),
        RemoteSandboxNetwork::AnyHost => Allowlist::any(),
        RemoteSandboxNetwork::Hosts(hosts) => {
            let mut patterns = Vec::with_capacity(hosts.len());
            for host in hosts {
                patterns.push(
                    HostPattern::parse(host)
                        .map_err(|error| anyhow!("invalid allowed host '{host}': {error}"))?,
                );
            }
            Allowlist::from_patterns(patterns)
        }
    };

    let (events_tx, _events_rx) = futures::channel::mpsc::unbounded();
    let config = ProxyConfig {
        allowlist,
        upstream: None,
        events: events_tx,
        policy,
        // An SSH host gets no editor-wide allowlist, so there the per-command
        // allowlist is the whole policy (documented boundary in the ADR
        // self-audit).
        outer_policy: network_ceiling.map(|ceiling| {
            let ceiling: Arc<dyn DestinationPolicy> = ceiling.clone();
            ceiling
        }),
    };
    // TCP on 127.0.0.1 (not a unix socket): slirp forwards `10.0.2.2:<port>`
    // to the host's `127.0.0.1:<port>`.
    ProxyHandle::spawn(config).map_err(|error| anyhow!("failed to start egress proxy: {error:#}"))
}

// ===========================================================================
// Filesystem backend selection
// ===========================================================================

#[derive(Clone, Copy)]
enum FsBackend {
    /// Landlock LSM, with the detected ABI version (>= 1).
    Landlock(i32),
    /// In-namespace mount jail (`pivot_root`).
    MountJail,
}

/// Pick the filesystem-isolation backend for this kernel: Landlock if the LSM is
/// available (kernel >= 5.13, so 24.04), else the mount jail (5.4, so 18.04). If
/// neither is possible we fail closed — there is no unsandboxed fallback.
///
/// WSL mode accepts Landlock only. WSL mounts every Windows drive as its own
/// `/mnt/<drive>` filesystem, and the mount jail's read-only remount of `/` does
/// not reach submounts, so under it the Windows drives would stay writable.
fn select_fs_backend(require_landlock: bool) -> Result<FsBackend> {
    let abi = landlock_abi();
    if abi >= 1 {
        return Ok(FsBackend::Landlock(abi));
    }
    if require_landlock {
        bail!("the kernel does not provide Landlock");
    }
    // Mount-jail viability can't be fully probed without attempting it; the
    // holder fails closed if the mounts are refused. We still require user
    // namespaces to be usable at all — that's checked when the holder unshares.
    Ok(FsBackend::MountJail)
}

// ===========================================================================
// Landlock (raw syscalls; libc has the numbers but not the structs/consts)
// ===========================================================================

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

// The read/execute accesses are intentionally never *handled* (so reads and
// execs stay allowed everywhere); they are kept here as a complete, documented
// reference of the ABI-1 access set.
#[allow(
    dead_code,
    reason = "documents the read/execute accesses we leave unhandled"
)]
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
#[allow(
    dead_code,
    reason = "documents the read/execute accesses we leave unhandled"
)]
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
#[allow(
    dead_code,
    reason = "documents the read/execute accesses we leave unhandled"
)]
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13; // ABI 2
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5
const LANDLOCK_SCOPE_SIGNAL: u64 = 1 << 1; // ABI 6

/// `struct landlock_ruleset_attr` as of ABI 6. A kernel that knows fewer fields
/// accepts the longer struct as long as the fields it does not know are zero,
/// so `handled_access_net` stays 0 and `scoped` is set only on ABI >= 6.
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// `struct landlock_path_beneath_attr`, which the kernel declares `packed`
/// (12 bytes: a `u64` immediately followed by an `s32`).
#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

fn landlock_abi() -> i32 {
    // SAFETY: querying the ABI with a NULL attr and size 0 is the documented
    // probe and touches no memory.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ret < 0 { 0 } else { ret as i32 }
}

/// The write-type accesses to *handle* (restrict), masked to what the detected
/// ABI understands. Read/execute accesses are deliberately left unhandled, so
/// reads and execs stay allowed everywhere.
fn landlock_handled_write_access(abi: i32) -> u64 {
    let mut access = LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        access |= LANDLOCK_ACCESS_FS_REFER;
    }
    if abi >= 3 {
        access |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        access |= LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    access
}

/// The subset that applies to a single **non-directory** inode (a regular file,
/// a symlink target, or a `/dev` node), so e.g. a `>file.txt` truncate or a
/// `>/dev/null` redirect works. Deliberately omits every directory-only access
/// (`MAKE_*`, `REMOVE_*`, `READ_DIR`, `REFER`): Landlock rejects a `PATH_BENEATH`
/// rule with `EINVAL` if it grants any of those on the fd of a non-directory.
/// `READ_FILE` is likewise omitted — reads are never *handled*, so re-granting
/// them would exceed the ruleset's handled set (also `EINVAL`).
fn landlock_file_write_access(abi: i32) -> u64 {
    let mut access = LANDLOCK_ACCESS_FS_WRITE_FILE;
    if abi >= 3 {
        access |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        access |= LANDLOCK_ACCESS_FS_IOCTL_DEV;
    }
    access
}

/// The write accesses to *allow* on one pinned entry, chosen from whether it is
/// a directory and clamped to the ruleset's `handled` mask.
///
/// A directory can carry the full handled write set (create/remove/rename
/// entries beneath it, write its files); a **non-directory** — a regular file, a
/// symlink target, or a device node — may carry only the file-type accesses,
/// because Landlock returns `EINVAL` from `landlock_add_rule` when a
/// directory-only access (`MAKE_*`, `REMOVE_*`, `READ_DIR`, `REFER`) is granted
/// on a non-directory fd. This is the exact trap the writable-root child
/// enumeration hit: granting the whole handled set on a plain `file.txt` sitting
/// beside a protected `.git` failed closed and blocked every command.
///
/// The result is intersected with `handled` so it can never contain a bit the
/// ruleset did not handle (that too is an `EINVAL` source, independent of inode
/// type).
fn landlock_entry_write_access(is_dir: bool, abi: i32, handled: u64) -> u64 {
    let requested = if is_dir {
        landlock_handled_write_access(abi)
    } else {
        landlock_file_write_access(abi)
    };
    requested & handled
}

/// Whether the inode behind `fd` is a directory. `fd` may be an `O_PATH` handle
/// (`fstat` is one of the few operations `O_PATH` permits), which is what the
/// Landlock rule path opens — so the type is read from the *same* fd the rule
/// pins, leaving no window for the object to change type between the check and
/// the rule.
fn fd_is_dir(fd: RawFd) -> Result<bool> {
    let stat = nix::sys::stat::fstat(fd).map_err(|errno| anyhow!("fstat failed: {errno}"))?;
    Ok(stat.st_mode & libc::S_IFMT == libc::S_IFDIR)
}

/// Open an `O_PATH` handle pinning the inode named by `path` (following
/// symlinks), returning it with its verified canonical path.
fn pin_path(path: &Path) -> Result<CanonicalPathBuf> {
    CanonicalPathBuf::resolve(path).with_context(|| format!("pinning {}", path.display()))
}

/// A protected path as the carve and the read-only binds see it.
struct ProtectedEntry {
    /// Where the entry itself lives: its parent resolved through symlinks,
    /// then its own name. For a symlink that is the link, not what it points
    /// at; for a missing entry, where it would be created.
    location: PathBuf,
    kind: ProtectedKind,
}

enum ProtectedKind {
    /// A file or directory, pinned.
    Present(CanonicalPathBuf),
    /// A symlink, with its target pinned. The link must not be replaceable and
    /// the target not writable; a bind, which follows the link, would only
    /// cover the target.
    Symlink(CanonicalPathBuf),
    /// Nothing there yet, and it has to stay that way: creating
    /// `~/.bash_login` or a worktree's `.git` is as good as writing one.
    Missing,
}

impl ProtectedEntry {
    fn pin(&self) -> Option<&CanonicalPathBuf> {
        match &self.kind {
            ProtectedKind::Present(pin) | ProtectedKind::Symlink(pin) => Some(pin),
            ProtectedKind::Missing => None,
        }
    }

    /// What the carve has to reach for this entry: the entry itself, and for a
    /// symlink also its target.
    fn carve_targets(&self) -> impl Iterator<Item = &Path> {
        let target = match &self.kind {
            ProtectedKind::Symlink(target) => Some(target.path()),
            ProtectedKind::Present(_) | ProtectedKind::Missing => None,
        };
        std::iter::once(self.location.as_path()).chain(target)
    }
}

/// Classify and pin every protected path, **failing closed** when one exists
/// but cannot be pinned.
///
/// Anything but "nothing is there" that stops a pin aborts the command, because
/// a dropped protected entry is not a missing restriction — it is a *granted*
/// one: with the pin silently discarded the worktree root looked free of
/// protected descendants and received one recursive write grant, which made
/// `.git/PWNED` creatable (a `chmod 0300 .git` was enough to trigger it before
/// `O_PATH` was restored). That covers an unsearchable parent, which reports
/// `PermissionDenied` (we cannot tell what is there), and a **dangling
/// symlink**, which is not absent: a write through it creates its target, and
/// for `~/.bashrc -> dotfiles/bashrc` inside a writable home directory that
/// runs at the next login.
///
/// A path below a plain file is dropped: nothing can ever be created there.
fn classify_protected_paths(paths: &[PathBuf]) -> Result<Vec<ProtectedEntry>> {
    let mut entries = Vec::with_capacity(paths.len());
    for path in paths {
        let kind = match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                let pin = pin_path(path)
                    .map_err(|error| anyhow!("{}: {}", path.display(), error.root_cause()))?;
                if metadata.file_type().is_symlink() {
                    ProtectedKind::Symlink(pin)
                } else {
                    ProtectedKind::Present(pin)
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProtectedKind::Missing,
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => continue,
            Err(error) => bail!("{}: {error}", path.display()),
        };
        let location = match &kind {
            ProtectedKind::Present(pin) => pin.path().to_path_buf(),
            ProtectedKind::Symlink(_) | ProtectedKind::Missing => entry_location(path)?,
        };
        entries.push(ProtectedEntry { location, kind });
    }
    Ok(entries)
}

/// `path` with everything but its last component resolved through symlinks,
/// from its deepest existing ancestor (missing directories below that are kept
/// as written).
fn entry_location(path: &Path) -> Result<PathBuf> {
    let unlocatable = || anyhow!("{} names no entry of a directory", path.display());
    let name = path.file_name().ok_or_else(unlocatable)?;
    let mut ancestor = path.parent().ok_or_else(unlocatable)?;
    let mut missing: Vec<&OsStr> = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut location) => {
                location.extend(missing.iter().rev());
                location.push(name);
                return Ok(location);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or_else(unlocatable)?);
                ancestor = ancestor.parent().ok_or_else(unlocatable)?;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error))
                    .with_context(|| format!("resolving {}", ancestor.display()));
            }
        }
    }
}

/// Enforce the planned `grants`, plus the locations that are always writable:
/// harmless device nodes and `temporary_writable` (the command's private temp
/// directories). `scope_signals` additionally stops the command from
/// signalling processes outside the sandbox (ABI 6; silently unavailable
/// before that). Bubblewrap got the same effect from its PID namespace.
fn apply_landlock(
    abi: i32,
    grants: &[CanonicalPathBuf],
    temporary_writable: &[PathBuf],
    scope_signals: bool,
) -> Result<()> {
    // No-new-privs is a precondition for `landlock_restrict_self`.
    set_no_new_privs()?;

    let handled = landlock_handled_write_access(abi);
    let attr = LandlockRulesetAttr {
        handled_access_fs: handled,
        handled_access_net: 0,
        scoped: if scope_signals && abi >= 6 {
            LANDLOCK_SCOPE_SIGNAL
        } else {
            0
        },
    };
    // SAFETY: `attr` is a valid, correctly-sized ruleset attr; flags 0 creates a
    // ruleset (the VERSION flag is only for the ABI probe).
    let ruleset_fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };
    if ruleset_fd < 0 {
        bail!(
            "landlock_create_ruleset failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let ruleset_fd = ruleset_fd as RawFd;
    // Own it so it is closed on every exit path.
    // SAFETY: freshly returned by the kernel, owned solely by us.
    let ruleset = unsafe { OwnedFd::from_raw_fd(ruleset_fd) };

    for grant in grants {
        // One entry that cannot take a rule must not fail the whole command
        // closed (that is how an `EINVAL` on plain files once blocked every
        // command on 24.04): it is skipped, which denies writes there and
        // nothing else.
        if let Err(error) = add_pin_rule(ruleset.as_raw_fd(), grant, handled, abi) {
            log::warn!(
                "[remote-sandbox] not granting write on {}: {error:#}",
                grant.path().display()
            );
        }
    }

    // Always allow the standard ephemeral write locations. These are character
    // devices (non-directories), so the granted access is narrowed to the
    // file-type subset for them (a directory-only access here would be
    // rejected with `EINVAL`, just as it was for the writable-root children).
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/random",
        "/dev/urandom",
    ] {
        // A missing node is simply skipped.
        if let Err(error) = add_path_rule(ruleset.as_raw_fd(), Path::new(dev), handled, abi) {
            log::debug!("[remote-sandbox] not granting write on {dev}: {error:#}");
        }
    }
    // Private temp locations, never the shared `/tmp`: granting that wholesale
    // would recursively re-expose any writable or protected path below it (a
    // `.git` inside a `/tmp` worktree). In WSL mode `/tmp` itself is a private
    // tmpfs, set up so that this holds (see `provide_private_tmp`).
    for path in temporary_writable {
        add_path_rule(ruleset.as_raw_fd(), path, handled, abi)
            .with_context(|| format!("allowing write on {}", path.display()))?;
    }

    // Enforce.
    // SAFETY: `ruleset` is a valid ruleset fd; flags 0.
    let ret = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) };
    if ret != 0 {
        bail!(
            "landlock_restrict_self failed: {}",
            std::io::Error::last_os_error()
        );
    }
    drop(ruleset);
    Ok(())
}

/// The Landlock write grants and read-only binds that confine one command,
/// worked out before the `fork`. The parent keeps every pin open until the
/// command has exited: on WSL's DrvFs an inode nobody references is looked up
/// as a new one the next time, which the rule on the old one does not cover —
/// without this the command lost write access to its worktree as soon as the
/// holder's descriptors closed at `exec`.
#[derive(Default)]
struct FilesystemPlan {
    /// Granted write, each with everything below it.
    grants: Vec<CanonicalPathBuf>,
    /// Protected entries bound read-only over themselves (WSL mode).
    binds: Vec<CanonicalPathBuf>,
}

/// Plan the grants for the writable roots (all of `/` under `allow_fs_write`),
/// carving around the protected entries.
///
/// Landlock only ever adds access, and a right on a directory covers
/// everything below it, so an entry inside a writable root is kept out by
/// granting the root's entries one by one, skipping it and descending into any
/// entry that leads to it (see [`plan_subtree`]). Every directory on that
/// chain then neither takes new entries nor gives any up. That is what stops a
/// command from renaming `~/.cache` away and recreating
/// `~/.cache/zed-jp-enhance/sandbox/<key>/zed-remote-server` with a binary of
/// its own, from creating a missing `~/.bash_login` or `.git`, and from
/// replacing a protected symlink. Existing entries of a carved directory, and
/// everything below them, stay writable.
///
/// With `use_binds` (WSL mode, which has a mount namespace of its own) an
/// existing entry directly inside a root is bound read-only over itself
/// instead, which keeps the root whole: new files at a worktree root beside its
/// `.git`, as Bubblewrap allowed. Renaming it is refused because it is a mount
/// point, and above it there is nothing left to rename but the root. Only
/// where `names_are_exact` holds for the root, though (see
/// [`directory_names_are_exact`]), and never for an entry that holds another
/// writable root (see [`holds_writable_root`]).
fn plan_filesystem(
    writable_pins: &[CanonicalPathBuf],
    protected: &[ProtectedEntry],
    allow_fs_write: bool,
    use_binds: bool,
    names_are_exact: &dyn Fn(&CanonicalPathBuf) -> bool,
) -> Result<FilesystemPlan> {
    let roots = if allow_fs_write {
        vec![pin_path(Path::new("/"))?]
    } else {
        writable_pins.to_vec()
    };
    let mut plan = FilesystemPlan::default();
    for root in &roots {
        let bind_children = use_binds && names_are_exact(root);
        let mut carve_targets: Vec<&Path> = Vec::new();
        for entry in protected {
            if let ProtectedKind::Present(pin) = &entry.kind
                && bind_children
                && entry.location.parent() == Some(root.path())
                && !holds_writable_root(&entry.location, writable_pins)
            {
                if !plan.binds.contains(pin) {
                    plan.binds.push(pin.clone());
                }
                continue;
            }
            carve_targets.extend(
                entry
                    .carve_targets()
                    .filter(|target| target.starts_with(root.path()) && *target != root.path()),
            );
        }
        plan_subtree(root, &carve_targets, protected, &mut plan.grants, 0)?;
    }
    Ok(plan)
}

/// Whether another writable root lies inside the protected entry at
/// `location`. A read-only bind of the entry would make that root read-only
/// too — the WSL home opened as a project holds `~/.zed-jp-enhance-claude`,
/// and the sandboxed Claude Code session's own configuration below it — so
/// such an entry is carved instead: no grant reaches it from the outer root,
/// and the inner root keeps its own.
fn holds_writable_root(location: &Path, writable_pins: &[CanonicalPathBuf]) -> bool {
    writable_pins
        .iter()
        .any(|pin| pin.path() != location && pin.path().starts_with(location))
}

/// Pin the writable roots for either backend.
///
/// A root that cannot be pinned is skipped: it simply is not granted, which
/// only takes access away. In WSL mode (`require_canonical`) each root must
/// also still be the canonical path the Windows side resolved and the user
/// approved; one that now leads somewhere else (a component swapped for a
/// symlink since) fails the command closed instead of being granted wherever
/// it points now.
fn pin_writable_paths(paths: &[PathBuf], require_canonical: bool) -> Result<Vec<CanonicalPathBuf>> {
    let mut pins = Vec::with_capacity(paths.len());
    for path in paths {
        let pinned = if require_canonical {
            CanonicalPathBuf::from_canonical(path.clone())
                .with_context(|| format!("verifying writable path {}", path.display()))
        } else {
            pin_path(path)
        };
        match pinned {
            Ok(pin) => pins.push(pin),
            Err(error) => {
                let missing = std::fs::symlink_metadata(path)
                    .err()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
                if require_canonical && !missing {
                    return Err(error);
                }
                log::warn!(
                    "[remote-sandbox] skipping writable path {}: {error:#}",
                    path.display()
                );
            }
        }
    }
    Ok(pins)
}

/// One level of [`plan_filesystem`]'s carve: grant `dir_pin` whole unless a
/// carve target lies below it, else grant its entries one by one. `depth` is 0
/// for the writable root itself.
fn plan_subtree(
    dir_pin: &CanonicalPathBuf,
    carve_targets: &[&Path],
    protected: &[ProtectedEntry],
    grants: &mut Vec<CanonicalPathBuf>,
    depth: usize,
) -> Result<()> {
    let dir = dir_pin.path();
    // The directory itself is protected: grant nothing. The recursion never
    // descends into a protected entry, so this only fires when the *writable
    // root* is one — in which case protection wins over the write grant.
    if protected.iter().any(|entry| entry.pin() == Some(dir_pin)) {
        log::warn!(
            "[remote-sandbox] granting no write on {}: it is a protected path",
            dir.display()
        );
        return Ok(());
    }
    if !carve_targets
        .iter()
        .any(|target| target.starts_with(dir) && *target != dir)
    {
        grants.push(dir_pin.clone());
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Below the root, an unlistable directory grants nothing rather than
        // failing the command: granting it wholesale is the one thing we must
        // not do (it would re-expose the protected path underneath), and the
        // command can still be perfectly able to do its job without writing
        // there. The root itself is different — if the worktree cannot be
        // listed, nothing sensible can be granted at all.
        Err(error) if depth > 0 => {
            log::warn!(
                "[remote-sandbox] granting no write under {} (cannot list it: {error})",
                dir.display()
            );
            return Ok(());
        }
        Err(error) => {
            return Err(anyhow::Error::new(error))
                .with_context(|| format!("reading {}", dir.display()));
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!(
                    "[remote-sandbox] skipping an entry of {} ({error})",
                    dir.display()
                );
                continue;
            }
        };
        let child = entry.path();
        let Ok(child_pin) = pin_path(&child) else {
            continue;
        };
        if protected
            .iter()
            .any(|protected| protected.pin() == Some(&child_pin))
        {
            continue;
        }
        // `pin_path` follows symlinks, so a symlink child resolves to whatever
        // it points at. Only an entry that really lives *below* this directory
        // is granted or descended into. That keeps two things true at once:
        // granting `link -> /` would hand write access to the whole filesystem
        // (something the uncarved single rule on the root never does, since
        // Landlock decides on the resolved inode), and descending through
        // `link -> ..` would recurse forever. A symlink whose target is inside
        // the worktree stays writable through the target's own grant.
        if !child_pin.path().starts_with(dir) || child_pin.path() == dir {
            log::warn!(
                "[remote-sandbox] not granting write on {} (it resolves to {}, outside {})",
                child.display(),
                child_pin.path().display(),
                dir.display()
            );
            continue;
        }
        // An entry leading to a carve target is carved in turn instead of being
        // granted wholesale — this is what keeps `~/.config/gh` and
        // `~/.local/share/zed-jp-enhance` protected while `~/.config` and
        // `~/.local` are inside a writable home directory.
        if carve_targets
            .iter()
            .any(|target| target.starts_with(child_pin.path()))
        {
            plan_subtree(&child_pin, carve_targets, protected, grants, depth + 1)?;
            continue;
        }
        grants.push(child_pin);
    }
    Ok(())
}

/// Add a `LANDLOCK_RULE_PATH_BENEATH` rule granting write beneath `path`, for
/// the few fixed locations that are not planned (device nodes, the private
/// temp directories).
fn add_path_rule(ruleset_fd: RawFd, path: &Path, handled: u64, abi: i32) -> Result<()> {
    // `open_o_path` rather than `OpenOptions::custom_flags`: std masks custom
    // flags with `!O_ACCMODE`, and musl's `O_ACCMODE` contains `O_PATH` — see
    // the note on `open_o_path`. Without it this became a real read-open, which
    // blocks forever on a FIFO.
    let fd = crate::util::open_o_path(path, crate::util::FollowSymlinks::Yes)
        .with_context(|| format!("opening {}", path.display()))?;
    add_fd_rule(ruleset_fd, fd.as_raw_fd(), path, handled, abi)
}

/// Add the rule for a planned grant, on the very inode that was pinned, so no
/// path is re-resolved at enforcement time.
fn add_pin_rule(ruleset_fd: RawFd, pin: &CanonicalPathBuf, handled: u64, abi: i32) -> Result<()> {
    add_fd_rule(ruleset_fd, pin.fd().as_raw_fd(), pin.path(), handled, abi)
}

/// The access is chosen from the inode's type via
/// [`landlock_entry_write_access`]: a directory receives the full handled write
/// set, a non-directory only file-type accesses — otherwise Landlock rejects the
/// rule with `EINVAL`. `handled` is the ruleset's handled mask and `abi` the
/// detected Landlock ABI (both bound the accesses that can be requested).
fn add_fd_rule(ruleset_fd: RawFd, fd: RawFd, path: &Path, handled: u64, abi: i32) -> Result<()> {
    let is_dir =
        fd_is_dir(fd).with_context(|| format!("determining inode type of {}", path.display()))?;
    let access = landlock_entry_write_access(is_dir, abi, handled);
    let attr = LandlockPathBeneathAttr {
        allowed_access: access,
        parent_fd: fd,
    };
    // SAFETY: `attr` is a valid path-beneath attr; the fd stays open across the
    // call.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const LandlockPathBeneathAttr,
            0u32,
        )
    };
    if ret != 0 {
        bail!(
            "landlock_add_rule for {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Basename prefixes of the two per-command scratch directories the backends
/// create under `/tmp`, each suffixed with the holder's pid: the Landlock
/// backend's private `TMPDIR`, and the mount jail's `pivot_root` target (which
/// survives as an empty directory on the host once the holder's mount namespace
/// is gone).
const TEMP_DIR_PREFIXES: [&str; 2] = ["zed-remote-sandbox-", ".zed-remote-jail-"];

/// The scratch directories a holder with `pid` may have created.
fn command_temp_dirs(pid: libc::pid_t) -> [PathBuf; 2] {
    TEMP_DIR_PREFIXES.map(|prefix| PathBuf::from(format!("/tmp/{prefix}{pid}")))
}

/// Remove one scratch directory, but only when it really is a directory we own.
///
/// `/tmp` is world-writable (sticky), so another local user can plant a symlink
/// or a directory at a predictable name; checking with `symlink_metadata`
/// (which does not follow) before removing keeps this from ever deleting
/// something else. `remove_dir_all` itself refuses to traverse a symlink.
fn remove_owned_temp_dir(dir: &Path) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(metadata) = std::fs::symlink_metadata(dir) else {
        return;
    };
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return;
    }
    if let Err(error) = std::fs::remove_dir_all(dir) {
        log::warn!(
            "[remote-sandbox] could not remove scratch dir {}: {error}",
            dir.display()
        );
    }
}

/// Remove the scratch directories belonging to a finished holder. Called by the
/// parent, which is the only participant still alive and unrestricted once the
/// command has exited (the holder `exec`s the command and, under Landlock,
/// could not delete them afterwards anyway).
fn remove_command_temp_dirs(pid: libc::pid_t) {
    for dir in command_temp_dirs(pid) {
        remove_owned_temp_dir(&dir);
    }
}

/// Sweep scratch directories left by earlier helper runs whose process is gone
/// — one `read_dir` of `/tmp` at startup. Without this, a killed helper (or any
/// crash between `create_private_tmpdir` and the parent's cleanup) leaks a
/// directory per command, which matters on the small disks these remotes have.
/// Best-effort by design: nothing here may fail the command.
fn sweep_stale_temp_dirs() {
    let Ok(entries) = std::fs::read_dir("/tmp") else {
        return;
    };
    let self_pid = std::process::id();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(suffix) = TEMP_DIR_PREFIXES
            .iter()
            .find_map(|prefix| name.strip_prefix(prefix))
        else {
            continue;
        };
        let Ok(pid) = suffix.parse::<u32>() else {
            continue;
        };
        // Leave anything a live process might still be using, including our own
        // (a second helper can share our pid only after we are gone).
        if pid == self_pid || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        remove_owned_temp_dir(&entry.path());
    }
}

/// Create a private, owner-only per-command temp directory (for the Landlock
/// backend's `TMPDIR`; mount-jail uses its own private tmpfs at `/tmp` instead).
fn create_private_tmpdir(scratch_pid: libc::pid_t) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = PathBuf::from(format!("/tmp/zed-remote-sandbox-{scratch_pid}"));
    // Clear a stale dir left by a recycled pid; only succeeds for one we own.
    remove_owned_temp_dir(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating private temp dir {}", dir.display()))?;
    Ok(dir)
}

/// Raise the soft open-file limit to the hard one, returning the original. The
/// parent holds every planned grant open while the command runs (see
/// [`FilesystemPlan`]), which in a large home directory can take more
/// descriptors than the default soft limit allows. The command gets the
/// original back ([`restore_open_file_limit`]).
fn raise_open_file_limit() -> Option<libc::rlimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid out-parameter for the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    let original = limit;
    limit.rlim_cur = limit.rlim_max;
    // SAFETY: `limit` is a valid, initialized `rlimit`.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        log::warn!(
            "[remote-sandbox] could not raise the open-file limit: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }
    Some(original)
}

fn restore_open_file_limit(original: Option<libc::rlimit>) {
    let Some(original) = original else {
        return;
    };
    // SAFETY: `original` is the valid `rlimit` `raise_open_file_limit` read.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &original) } != 0 {
        log::warn!(
            "[remote-sandbox] could not restore the open-file limit: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn set_no_new_privs() -> Result<()> {
    // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)` takes only scalar args.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        bail!(
            "prctl(PR_SET_NO_NEW_PRIVS) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

// ===========================================================================
// WSL mode (ADR-F46): read-only binds, capabilities, and the two utilities
// ===========================================================================

/// Whether an entry of `directory` can be reached by exactly one spelling of
/// its name. A read-only bind protects one spelling only — a mount sits on the
/// dentry that was looked up — so it is a protection only where no other
/// spelling reaches the same object. On WSL's DrvFs one does: in a directory
/// Windows created, `.GIT/config` is `.git/config`, and a sandboxed command
/// appended to it, and renamed `.GIT` away, straight past a bind on `.git` (8.3
/// and case-folded Unicode names would do the same). A case-folding ext4 or
/// tmpfs directory is no different. Everywhere else the carve is used instead.
fn directory_names_are_exact(directory: &CanonicalPathBuf) -> bool {
    use nix::sys::statfs::{BTRFS_SUPER_MAGIC, EXT4_SUPER_MAGIC, TMPFS_MAGIC, fstatfs};
    let Ok(statistics) = fstatfs(directory.fd()) else {
        return false;
    };
    [EXT4_SUPER_MAGIC, BTRFS_SUPER_MAGIC, TMPFS_MAGIC].contains(&statistics.filesystem_type())
        && !directory_folds_case(directory.path())
}

/// Whether `directory` has the case-folding attribute (`chattr +F`).
fn directory_folds_case(directory: &Path) -> bool {
    const FS_CASEFOLD_FL: libc::c_long = 0x4000_0000;
    let Ok(handle) = std::fs::File::open(directory) else {
        // Unknown counts as folding, which only costs a carve.
        return true;
    };
    let mut flags: libc::c_long = 0;
    // SAFETY: `FS_IOC_GETFLAGS` writes at most a `long` into `flags`, which
    // outlives the call, and `handle` stays open across it.
    let status = unsafe { libc::ioctl(handle.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) };
    status == 0 && flags & FS_CASEFOLD_FL != 0
}

/// Give the command a mount namespace of its own, for the private `/tmp` and
/// `/dev/shm` and the read-only binds.
///
/// Must run before Landlock is enforced: from then on every mount operation is
/// refused, the command's included, so it cannot take any of this away again.
fn enter_private_mount_namespace() -> Result<()> {
    // SAFETY: `unshare` with this flag reads no memory.
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        bail!("unshare(CLONE_NEWNS): {}", std::io::Error::last_os_error());
    }
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .context("making / private")
}

/// Give the command a `/tmp` of its own, as Bubblewrap did: a fresh tmpfs over
/// the shared one with every writable root that lives below `/tmp` bound back
/// in at its own path — or, if the kernel refuses the tmpfs, the per-command
/// scratch directory bound over `/tmp`. Returns what `TMPDIR` should name.
///
/// The shared `/tmp` stays when the command may write there anyway
/// (`allow_fs_write`, or a writable root containing it), and when a writable
/// root below it had to be carved: the private `/tmp`'s own write rule would
/// reach everything mounted below it and undo the carve. The command then gets
/// the per-command directory as its `TMPDIR`, as on an SSH host.
fn provide_private_tmp(
    writable_pins: &[CanonicalPathBuf],
    plan: &FilesystemPlan,
    allow_fs_write: bool,
    temporary_writable: &mut Vec<PathBuf>,
    scratch_pid: libc::pid_t,
) -> Result<PathBuf> {
    let tmp = std::fs::canonicalize("/tmp").context("resolving /tmp")?;
    let shared_tmp_writable = allow_fs_write
        || writable_pins
            .iter()
            .any(|root| tmp.starts_with(root.path()));
    let mut roots_below: Vec<&CanonicalPathBuf> = writable_pins
        .iter()
        .filter(|root| root.path().starts_with(&tmp) && root.path() != tmp)
        .collect();
    let carved_below = roots_below.iter().any(|root| !plan.grants.contains(root));
    if shared_tmp_writable || carved_below {
        if carved_below {
            log::warn!(
                "[remote-sandbox] keeping the shared /tmp: a writable root below it is carved"
            );
        }
        let private_tmp = create_private_tmpdir(scratch_pid)?;
        temporary_writable.push(private_tmp.clone());
        return Ok(private_tmp);
    }

    // Reopened here, while `/tmp` still shows them: `mount` binds only from the
    // caller's own mount namespace, and the pins were opened in the parent's.
    // Only the outermost roots are bound; the others come along inside them.
    roots_below.sort_by_key(|root| root.path().components().count());
    let mut sources: Vec<(&CanonicalPathBuf, OwnedFd)> = Vec::new();
    for root in roots_below {
        if sources
            .iter()
            .any(|(outer, _)| root.path().starts_with(outer.path()))
        {
            continue;
        }
        let source = crate::util::open_o_path(root.path(), crate::util::FollowSymlinks::No)
            .with_context(|| format!("reopening writable {}", root.path().display()))?;
        match (
            crate::util::linux_fd_identity(source.as_raw_fd()),
            crate::util::linux_fd_identity(root.fd().as_raw_fd()),
        ) {
            (Some(reopened), Some(pinned)) if reopened == pinned => {}
            _ => bail!(
                "writable {} was replaced after it was pinned",
                root.path().display()
            ),
        }
        sources.push((root, source));
    }

    if let Err(error) = mount(
        Some(Path::new("tmpfs")),
        &tmp,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777"),
    ) {
        log::warn!(
            "[remote-sandbox] no private tmpfs for /tmp ({error:#}); \
             binding the per-command directory over it"
        );
        let private_tmp = create_private_tmpdir(scratch_pid)?;
        mount(
            Some(private_tmp.as_path()),
            &tmp,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .context("binding the per-command directory over /tmp")?;
    }
    for (root, source) in sources {
        let target = root.path();
        let mount_point = if fd_is_dir(source.as_raw_fd())? {
            std::fs::create_dir_all(target)
        } else {
            target
                .parent()
                .map_or(Ok(()), |parent| std::fs::create_dir_all(parent))
                .and_then(|()| std::fs::File::create(target).map(drop))
        };
        mount_point.with_context(|| format!("creating mount point {}", target.display()))?;
        let source_path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
        mount(
            Some(source_path.as_path()),
            target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .with_context(|| format!("binding {} back into the private /tmp", target.display()))?;
        verify_same_inode(root, target)?;
    }
    temporary_writable.push(tmp.clone());
    Ok(tmp)
}

/// The home whose read-hidden entries are covered: the account's own. The test
/// helper may point it at a fake home so the hiding can be exercised without
/// touching a real one; a production build ignores the override.
fn read_hidden_home(home: &Path) -> PathBuf {
    #[cfg(feature = "remote-sandbox-test")]
    if let Some(home) = std::env::var_os("ZED_SANDBOX_READ_HIDDEN_HOME_FOR_TESTS") {
        return PathBuf::from(home);
    }
    home.to_path_buf()
}

/// Where the Windows drives are in the command's mount namespace.
struct WindowsDrives {
    /// What gets covered: `/mnt` — WSL's default automount root, where Docker
    /// Desktop also binds each distro's root (`/mnt/wsl`) — and every drive
    /// mounted outside it (`[automount] root = /` puts them at `/c`, `/d`).
    covers: Vec<PathBuf>,
    /// Every drive's mount point, which Windows homes are found under.
    mounts: Vec<PathBuf>,
}

/// The Windows drives, found in `/proc/self/mountinfo` by filesystem type (see
/// [`drive_mount_points`]) rather than assumed at `/mnt`. The test helper may
/// point it at a fake tree, each directory of which stands for a drive, so the
/// hiding can be exercised without touching a real Windows home; a production
/// build ignores the override.
fn windows_drives() -> Result<WindowsDrives> {
    #[cfg(feature = "remote-sandbox-test")]
    if let Some(root) = std::env::var_os("ZED_WSL_HIDE_DRIVES_ROOT_FOR_TESTS") {
        let root = PathBuf::from(root);
        let mut mounts: Vec<PathBuf> = std::fs::read_dir(&root)
            .with_context(|| format!("listing the fake drives in {}", root.display()))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        mounts.sort();
        return Ok(WindowsDrives {
            covers: vec![root],
            mounts,
        });
    }
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .context("reading /proc/self/mountinfo to find the Windows drives")?;
    let mounts = drive_mount_points(&mountinfo);
    let mut covers: Vec<PathBuf> = Vec::new();
    let automount_root = Path::new("/mnt");
    if automount_root.is_dir() {
        covers.push(automount_root.to_path_buf());
    }
    for mount in &mounts {
        if !covers.iter().any(|cover| mount.starts_with(cover)) {
            covers.push(mount.clone());
        }
    }
    Ok(WindowsDrives { covers, mounts })
}

/// The mount points of the Windows drives in `mountinfo`, outermost first:
/// DrvFs over 9p (`aname=drvfs`), `drvfs` itself, and virtiofs tagged
/// `drvfs…`, which WSL can mount the drives with instead — the types
/// [`path_is_on_windows_fs`] tells a Windows file by. WSL shares its driver
/// store and GPU libraries over 9p and virtiofs as well, and covering those
/// would break GPU tools in the sandbox.
fn drive_mount_points(mountinfo: &str) -> Vec<PathBuf> {
    let mut drives: Vec<PathBuf> = Vec::new();
    for line in mountinfo.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        // Optional fields come between the mount options and a lone `-`.
        let Some(separator) = fields
            .iter()
            .skip(6)
            .position(|field| *field == "-")
            .map(|position| position + 6)
        else {
            continue;
        };
        let (Some(mount_point), Some(filesystem)) = (fields.get(4), fields.get(separator + 1))
        else {
            continue;
        };
        let source = fields.get(separator + 2).copied().unwrap_or_default();
        let options = fields.get(separator + 3).copied().unwrap_or_default();
        let drive = match *filesystem {
            "9p" => options
                .split(',')
                .any(|option| option.starts_with("aname=drvfs")),
            "drvfs" => true,
            "virtiofs" => source
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("drvfs")),
            _ => false,
        };
        let mount_point = unescape_mount_point(mount_point);
        if drive && !drives.contains(&mount_point) {
            drives.push(mount_point);
        }
    }
    drives.sort_by_key(|drive| drive.components().count());
    drives
}

/// A path as `/proc/self/mountinfo` writes it, where a space, tab, newline or
/// backslash is escaped as `\ooo`.
fn unescape_mount_point(field: &str) -> PathBuf {
    let mut unescaped = Vec::with_capacity(field.len());
    let mut rest = field.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        let escaped = tail
            .get(..3)
            .filter(|_| byte == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        match escaped {
            Some(escaped) => {
                unescaped.push(escaped);
                rest = tail.get(3..).unwrap_or_default();
            }
            None => {
                unescaped.push(byte);
                rest = tail;
            }
        }
    }
    PathBuf::from(OsStr::from_bytes(&unescaped))
}

/// ADR-F49 decision 11 on WSL: cover every place the Windows drives show (see
/// [`windows_drives`]) with an empty read-only tmpfs, and bind back only what
/// the command must reach there — its writable roots, both where the session
/// asked for them and where they resolved, and the directory it starts in (see
/// [`starting_directory`]). Whole trees rather than one cover per credential:
/// on case-insensitive DrvFs a path has many spellings
/// (`/mnt/c/users/u/appdata`, `.SSH`, 8.3 names), and Docker Desktop binds each
/// distro's root under `/mnt/wsl`, so covering names one by one can always be
/// walked around. Each mount made is added to `mounted`, for a caller to take
/// off again should a later one fail.
///
/// A writable root that would bring a Windows home's credentials back with it
/// (see [`windows_home_exposure`]) is refused with [`WindowsHomeRoot`] before
/// anything is mounted: hiding them inside it would need the Landlock grants
/// split around them as well, and a cover over one spelling of an entry leaves
/// its other spellings readable. Any other root that is one of the covered
/// trees or holds it leaves that tree as it is.
fn cover_windows_drives(
    writable_pins: &[CanonicalPathBuf],
    invocation: &Stage1Invocation,
    mounted: &mut Vec<PathBuf>,
) -> Result<()> {
    let drives = windows_drives()?;
    let mut spellings: Vec<PathBuf> = writable_pins
        .iter()
        .map(|pin| pin.path().to_path_buf())
        .chain(invocation.writable_paths.iter().cloned())
        .collect();
    spellings.sort();
    spellings.dedup();
    if let Some(refusal) = spellings
        .iter()
        .find_map(|path| windows_home_exposure(path, &drives.mounts))
    {
        return Err(refusal.into());
    }
    let covers: Vec<PathBuf> = drives
        .covers
        .into_iter()
        .filter(|cover| cover.is_dir() && !spellings.iter().any(|path| cover.starts_with(path)))
        .collect();
    if covers.is_empty() {
        return Ok(());
    }
    let approved: Vec<(u64, u64)> = writable_pins
        .iter()
        .filter_map(|pin| crate::util::linux_fd_identity(pin.fd().as_raw_fd()))
        .collect();
    spellings.retain(|path| covers.iter().any(|cover| path.starts_with(cover)));
    spellings.sort_by_key(|path| path.components().count());
    // Opened while the drives still show them, and bound back only when they
    // are a root the user approved: a spelling that leads somewhere else by now
    // is left out.
    let mut binds: Vec<BindBack> = Vec::new();
    for path in spellings {
        if binds.iter().any(|outer| path.starts_with(&outer.target)) {
            continue;
        }
        let Ok(source) = crate::util::open_o_path(&path, crate::util::FollowSymlinks::No) else {
            log::warn!(
                "[remote-sandbox] leaving {} out of the drives: it cannot be opened",
                path.display()
            );
            continue;
        };
        let approved_root = crate::util::linux_fd_identity(source.as_raw_fd())
            .is_some_and(|identity| approved.contains(&identity));
        if !approved_root {
            log::warn!(
                "[remote-sandbox] leaving {} out of the drives: it is not the writable root \
                 it was approved as",
                path.display()
            );
            continue;
        }
        binds.push(BindBack {
            target: path,
            source,
            read_only: false,
        });
    }
    if let Some(start) = starting_directory(invocation, &covers, &drives.mounts, &binds)? {
        binds.push(start);
    }
    // Outermost first, so a writable root inside the starting directory is
    // bound over it rather than hidden under it.
    binds.sort_by_key(|bind| bind.target.components().count());

    for cover in &covers {
        cover_and_bind_back(cover, &binds, mounted)?;
    }
    Ok(())
}

/// Mount an empty tmpfs over `cover`, bind each of `binds` inside it back
/// where it was, in the order given, then make the cover itself read-only;
/// what is bound back keeps its own access. Each mount made is added to
/// `mounted`.
fn cover_and_bind_back(cover: &Path, binds: &[BindBack], mounted: &mut Vec<PathBuf>) -> Result<()> {
    mount(
        Some(Path::new("tmpfs")),
        cover,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=755"),
    )
    .with_context(|| format!("covering {}", cover.display()))?;
    mounted.push(cover.to_path_buf());
    for bind in binds.iter().filter(|bind| bind.target.starts_with(cover)) {
        let target = &bind.target;
        // One inside the starting directory has its mount point there
        // already, and making it again could empty a file of the user's.
        let mount_point = if target.symlink_metadata().is_ok() {
            Ok(())
        } else if fd_is_dir(bind.source.as_raw_fd())? {
            std::fs::create_dir_all(target)
        } else {
            target
                .parent()
                .map_or(Ok(()), |parent| std::fs::create_dir_all(parent))
                .and_then(|()| std::fs::File::create(target).map(drop))
        };
        mount_point.with_context(|| format!("creating mount point {}", target.display()))?;
        let source_path = PathBuf::from(format!("/proc/self/fd/{}", bind.source.as_raw_fd()));
        mount(
            Some(source_path.as_path()),
            target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .with_context(|| {
            format!(
                "binding {} back over the cover on {}",
                target.display(),
                cover.display()
            )
        })?;
        mounted.push(target.clone());
        if bind.read_only {
            remount_read_only(target)
                .with_context(|| format!("making {} read-only", target.display()))?;
        }
    }
    // Read-only once the mount points are made, so nothing can be put in the
    // cover itself; what is bound back keeps its own access.
    mount(
        None,
        cover,
        None,
        libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
        None,
    )
    .with_context(|| format!("making the cover over {} read-only", cover.display()))?;
    Ok(())
}

/// Something bound back over a cover (see [`cover_and_bind_back`]).
struct BindBack {
    target: PathBuf,
    source: OwnedFd,
    read_only: bool,
}

/// A root that would bring a Windows home's credentials back with it, which a
/// command in WSL mode may not be given (see [`cover_windows_drives`]). Says
/// what it is or holds.
#[derive(Debug)]
struct WindowsHomeRoot(String);

impl std::fmt::Display for WindowsHomeRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}, whose credentials the sandbox keeps unreadable",
            self.0
        )
    }
}

impl std::error::Error for WindowsHomeRoot {}

/// The directory the command starts in, when it lies on a covered drive
/// outside every writable root, opened to be bound back there — read-only
/// unless every write is allowed — so a session that may not write there can
/// still start in it. One that would bring a Windows home's credentials back
/// with it (see [`windows_home_exposure`]) is refused as a writable root would
/// be; one inside a home's read-hidden entry (see
/// [`inside_hidden_windows_entry`]) is left covered, and the command does not
/// start.
fn starting_directory(
    invocation: &Stage1Invocation,
    covers: &[PathBuf],
    drives: &[PathBuf],
    binds: &[BindBack],
) -> Result<Option<BindBack>> {
    let Some(Ok(directory)) = invocation.chdir.as_deref().map(std::fs::canonicalize) else {
        return Ok(None);
    };
    if !covers.iter().any(|cover| directory.starts_with(cover))
        || binds.iter().any(|bind| directory.starts_with(&bind.target))
    {
        return Ok(None);
    }
    if let Some(refusal) = windows_home_exposure(&directory, drives) {
        return Err(refusal.into());
    }
    if inside_hidden_windows_entry(&directory, drives) {
        log::warn!(
            "[remote-sandbox] leaving {} covered: it is inside a Windows home's read-hidden \
             entries",
            directory.display()
        );
        return Ok(None);
    }
    let Ok(source) = crate::util::open_o_path(&directory, crate::util::FollowSymlinks::No) else {
        return Ok(None);
    };
    // What was opened is what gets bound, so it has to be what was checked: a
    // directory moved in between is left covered.
    let opened = std::fs::read_link(format!("/proc/self/fd/{}", source.as_raw_fd()));
    if !opened.is_ok_and(|opened| opened == directory) || !fd_is_dir(source.as_raw_fd())? {
        return Ok(None);
    }
    Ok(Some(BindBack {
        target: directory,
        source,
        read_only: !invocation.allow_fs_write,
    }))
}

/// Why `path` may not be bound back whole over the covered drives, if it may
/// not: it is a Windows home or holds one (see [`holds_windows_home`]), or,
/// inside a home, it holds one of the home's read-hidden entries (see
/// [`held_hidden_entry`]).
fn windows_home_exposure(path: &Path, drives: &[PathBuf]) -> Option<WindowsHomeRoot> {
    if holds_windows_home(path, drives) {
        return Some(WindowsHomeRoot(format!(
            "{} is a Windows home or holds one",
            path.display()
        )));
    }
    let entry = held_hidden_entry(path, drives)?;
    Some(WindowsHomeRoot(format!(
        "{} holds {} of the Windows home it is in",
        path.display(),
        entry.display()
    )))
}

/// Whether `path` is a Windows home or holds one: a drive with a `Users`
/// folder or anything above one, that folder, or a home (see
/// [`is_windows_home`]). A folder merely above a home kept outside `Users` is
/// not recognised.
fn holds_windows_home(path: &Path, drives: &[PathBuf]) -> bool {
    if drives
        .iter()
        .any(|drive| drive.starts_with(path) && drive.join("Users").is_dir())
    {
        return true;
    }
    let Some(drive) = drive_holding(path, drives) else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(drive) else {
        return false;
    };
    let components: Vec<&OsStr> = relative.iter().collect();
    match components.as_slice() {
        [folder] if folder.eq_ignore_ascii_case("users") => true,
        _ => is_windows_home(path, drive),
    }
}

/// The drive `path` lies on, below its mount point.
fn drive_holding<'a>(path: &Path, drives: &'a [PathBuf]) -> Option<&'a Path> {
    drives
        .iter()
        .filter(|drive| path.starts_with(drive) && path != drive.as_path())
        .max_by_key(|drive| drive.components().count())
        .map(PathBuf::as_path)
}

/// Whether `path`, on `drive`, is a Windows home: a folder in the drive's
/// `Users`, or one kept elsewhere that has both a profile's `NTUSER.DAT` and
/// its `AppData`. Compared without case, as DrvFs looks names up.
fn is_windows_home(path: &Path, drive: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(drive) else {
        return false;
    };
    let components: Vec<&OsStr> = relative.iter().collect();
    match components.as_slice() {
        [folder, _name] if folder.eq_ignore_ascii_case("users") => true,
        [] => false,
        _ => path.join("NTUSER.DAT").symlink_metadata().is_ok() && path.join("AppData").is_dir(),
    }
}

/// The Windows home on `drive` that `path` is, or lies in.
fn enclosing_windows_home<'a>(path: &'a Path, drive: &Path) -> Option<&'a Path> {
    path.ancestors()
        .take_while(|ancestor| *ancestor != drive)
        .find(|ancestor| is_windows_home(ancestor, drive))
}

/// The read-hidden entry of a Windows home that `path`, inside the home, holds
/// — `.config` holding `.config/gh`, `.docker` its `config.json` — whether or
/// not the entry exists yet, since a Windows program may make it while the
/// command runs. Compared by identity, like [`inside_hidden_windows_entry`].
fn held_hidden_entry(path: &Path, drives: &[PathBuf]) -> Option<PathBuf> {
    let drive = drive_holding(path, drives)?;
    let home = enclosing_windows_home(path.parent()?, drive)?;
    let identity = path_identity(path)?;
    read_hidden_home_paths(home, true)
        .into_iter()
        .find(|entry| {
            entry
                .ancestors()
                .skip(1)
                .take_while(|ancestor| *ancestor != home)
                .any(|ancestor| path_identity(ancestor) == Some(identity))
        })
}

/// Whether `directory`, on a covered drive, lies inside one of the read-hidden
/// entries (`AppData`, `.ssh`, …) of the Windows home it is in. Compared by
/// identity: on DrvFs a path can name an entry in any case, by its 8.3 name or
/// through a junction such as `Application Data`.
fn inside_hidden_windows_entry(directory: &Path, drives: &[PathBuf]) -> bool {
    let Some(home) =
        drive_holding(directory, drives).and_then(|drive| enclosing_windows_home(directory, drive))
    else {
        return false;
    };
    let hidden: Vec<(u64, u64)> = read_hidden_home_paths(home, true)
        .iter()
        .filter_map(|entry| path_identity(entry))
        .collect();
    directory
        .ancestors()
        .take_while(|ancestor| *ancestor != home)
        .any(|ancestor| path_identity(ancestor).is_some_and(|identity| hidden.contains(&identity)))
}

fn path_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// ADR-F49 decisions 8 and 15: the agent configurations a host agent acts on
/// (`.claude/`, `.mcp.json`, …), in each root the command may write that is a
/// worktree.
///
/// Judged after pinning, on where the root really is. A root is one agent's own
/// configuration directory — where those names are *not* protected, so the
/// agent can keep its own — only when it sits exactly where such a directory
/// sits *and* the client named it exactly there: a symlink planted in the
/// projects directory and pointed at a worktree would otherwise strip that
/// worktree's protections, and a root named through a symlink is treated as a
/// worktree here for the same reason `--wsl` mode insists a grant still be the
/// canonical path the user approved.
fn worktree_protected_additions(
    invocation: &Stage1Invocation,
    writable_pins: &[CanonicalPathBuf],
    home: &Path,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for pin in writable_pins {
        let root = pin.path();
        let named_where_it_lies = invocation
            .writable_paths
            .iter()
            .any(|requested| requested == root);
        if named_where_it_lies && is_sandboxed_agent_configuration(root, home) {
            continue;
        }
        for path in worktree_entries(root) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// What has to stay reachable however it lies among the read-hidden entries:
/// the roots the command may write, as the client named them (their pinned
/// canonical paths are added where the entries are split, see
/// [`read_hidden_targets`]), and in WSL mode the agent binaries staged for it.
///
/// **Nothing here is resolved again.** A path is kept because the user approved
/// it, or because the helper's own side put something there; resolving one now
/// would decide on what it points at *this moment*. A single symlink planted in
/// the staging directory (`agents -> ~/.ssh`, which a command with a write
/// grant there can make) would otherwise take the cover off a credential store
/// — for that command and every command on the host afterwards. The staged
/// directory is therefore kept only where it really is the directory it is
/// named as, and only on WSL, where the Windows side stages into it. Nothing
/// from the environment either: a variable a project can set would be another
/// way to un-hide a credential store.
fn reachable_paths(invocation: &Stage1Invocation, home: &Path) -> Vec<PathBuf> {
    let mut reachable = invocation.writable_paths.clone();
    if invocation.on_wsl {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        let agents = home.join(WSL_SANDBOX_HELPER_HOME_RELATIVE).join("agents");
        let is_a_real_directory = std::fs::symlink_metadata(&agents)
            .is_ok_and(|metadata| metadata.is_dir())
            && std::fs::canonicalize(&agents).is_ok_and(|canonical| canonical == agents);
        if is_a_real_directory {
            reachable.push(agents);
        }
    }
    reachable
}

/// ADR-F49 decision 14: where each of `requested` really lies, when every one
/// may be shown among the entries hidden from the command (see
/// [`resolve_readable_path`]). One that may not fails them all. A host that
/// refuses the hiding mounts shows everything anyway; the check still applies
/// there, so a request means the same wherever it runs.
fn resolve_readable_paths(
    requested: &[PathBuf],
    data_directory: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>> {
    requested
        .iter()
        .map(|path| {
            resolve_readable_path(path, data_directory, home)
                .map_err(|refusal| anyhow!("--readable {}: {refusal}", path.display()))
        })
        .collect()
}

/// Pin what may be shown, on the resolved paths the check above returned: the
/// cover binds back the very object that was checked, and a path that is not
/// where it was by then fails the command closed rather than being bound from
/// wherever it leads now.
fn pin_readable_paths(readable: &[PathBuf]) -> Result<Vec<CanonicalPathBuf>> {
    readable
        .iter()
        .map(|path| {
            CanonicalPathBuf::from_canonical(path.clone())
                .with_context(|| format!("pinning what may be read: {}", path.display()))
        })
        .collect()
}

/// The directories [`cover_enclosure`] covers whole when the command is granted
/// something inside one (see
/// [`crate::protected_paths::enclosed_directories_in`]). `data_directory` is the
/// env-derived one this process resolved ([`remote_agent_runtime_data_directory`]),
/// so the cover binds back exactly what the readable check accepted inside it.
fn enclosed_directories(home: &Path, data_directory: &Path) -> Vec<PathBuf> {
    crate::protected_paths::enclosed_directories_in(home, data_directory)
}

/// Those of `enclosures` to cover (see
/// [`crate::protected_paths::enclosures_to_cover`]). Anything else is left to
/// the split around the roots (see [`read_hidden_targets`]).
fn enclosures_to_cover(
    enclosures: &[PathBuf],
    writable_pins: &[CanonicalPathBuf],
    readable_pins: &[CanonicalPathBuf],
) -> Vec<PathBuf> {
    let granted: Vec<&Path> = writable_pins
        .iter()
        .chain(readable_pins)
        .map(|pin| pin.path())
        .collect();
    crate::protected_paths::enclosures_to_cover(enclosures, &granted)
}

/// What the read hiding does: the directories covered whole, and the entries
/// covered one by one beside them. Worked out twice from the same inputs — once
/// in the parent, where the command can still be refused, and once in init,
/// where the mounts are made.
fn read_hiding_plan(
    read_hidden: &[PathBuf],
    reachable: &[PathBuf],
    enclosures: &[PathBuf],
    writable_pins: &[CanonicalPathBuf],
    readable_pins: &[CanonicalPathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let enclosures = enclosures_to_cover(enclosures, writable_pins, readable_pins);
    // What lies in a covered directory is hidden by its cover, whether it is
    // there yet or not.
    let targets = read_hidden_targets(read_hidden, reachable, writable_pins)
        .into_iter()
        .filter(|target| {
            !enclosures
                .iter()
                .any(|enclosure| target.starts_with(enclosure))
        })
        .collect();
    (enclosures, targets)
}

/// ADR-F49 decisions 11 and 14: nothing the command is *shown* — what the
/// client asked to show, the staged agent binaries — may be or hold something
/// the hiding covers.
///
/// Where they overlap, the old answer was to leave that cover off, which hands
/// the command whatever the entry holds. The command is refused instead: a
/// credential store that one symlink can un-hide would stay un-hidden for every
/// command that follows, while a refusal is loud and fixable.
fn check_nothing_shown_is_covered(shown: &[PathBuf], targets: &[PathBuf]) -> Result<()> {
    for path in shown {
        let overlapping = targets
            .iter()
            .find(|target| path.starts_with(target) || target.starts_with(path));
        if let Some(target) = overlapping {
            bail!(
                "{} is shown to the command but overlaps {}, whose contents stay hidden",
                path.display(),
                target.display()
            );
        }
    }
    Ok(())
}

/// Cover `enclosure` with an empty read-only tmpfs and bind back only what the
/// command was granted inside it — the roots it may write, and what it may read
/// (read-only) — as [`cover_windows_drives`] does for the drives.
///
/// The sessions' configuration directory then shows a session its own project's
/// directory and nothing else, however many projects are added while it runs,
/// and this fork's data directory shows the agent installation and the Node.js
/// the command was given and nothing else of it; hiding the other entries one
/// by one could only hide those there at the start. Each location is opened
/// before the cover and bound back only when it is still the inode that was
/// pinned; one that is not stays covered, and the command does without it. Each
/// mount made is added to `mounted`.
fn cover_enclosure(
    enclosure: &Path,
    writable_pins: &[CanonicalPathBuf],
    readable_pins: &[CanonicalPathBuf],
    mounted: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut inside: Vec<(&CanonicalPathBuf, bool)> = writable_pins
        .iter()
        .map(|pin| (pin, false))
        .chain(readable_pins.iter().map(|pin| (pin, true)))
        .filter(|(pin, _)| pin.path() != enclosure && pin.path().starts_with(enclosure))
        .collect();
    inside.sort_by_key(|(pin, _)| pin.path().components().count());
    let mut binds: Vec<BindBack> = Vec::new();
    for (pin, read_only) in inside {
        let path = pin.path();
        if binds.iter().any(|outer| path.starts_with(&outer.target)) {
            continue;
        }
        let Ok(source) = crate::util::open_o_path(path, crate::util::FollowSymlinks::No) else {
            log::warn!(
                "[remote-sandbox] leaving {} covered: it cannot be opened",
                path.display()
            );
            continue;
        };
        let pinned = crate::util::linux_fd_identity(pin.fd().as_raw_fd());
        if pinned.is_none() || pinned != crate::util::linux_fd_identity(source.as_raw_fd()) {
            log::warn!(
                "[remote-sandbox] leaving {} covered: it was replaced after it was pinned",
                path.display()
            );
            continue;
        }
        binds.push(BindBack {
            target: path.to_path_buf(),
            source,
            read_only,
        });
    }
    cover_and_bind_back(enclosure, &binds, mounted)
}

/// Cover every read-hidden entry that exists with empty content so the
/// command cannot read it (ADR-F49 decision 11): the Linux home's
/// `read_hidden`, split around the roots kept inside one (see
/// [`read_hidden_targets`]) except inside `enclosures`, which are covered
/// whole around them (see [`cover_enclosure`]), and with `drives` the Windows
/// drives first (see [`cover_windows_drives`], for that invocation).
/// `needs_mount_namespace` enters one of the command's own first, which only
/// the backends that set up none of their own need — and only when there is
/// something to hide.
///
/// Landlock handles only writes, so reads of the whole filesystem stay allowed
/// there and read-only binds do not help: the entries are hidden by mounting
/// over them. All or none: when one cover is refused, the ones already made
/// come off again, since a set only partly covered would pass for protection
/// while leaving the rest readable. What a refusal means is the caller's
/// decision (see [`read_hiding_refused`]); the error says what state it left.
#[allow(
    clippy::too_many_arguments,
    reason = "hiding needs every part of the policy that decides what stays visible"
)]
fn hide_read_hidden_entries(
    read_hidden: &[PathBuf],
    reachable: &[PathBuf],
    enclosures: &[PathBuf],
    writable_pins: &[CanonicalPathBuf],
    readable_pins: &[CanonicalPathBuf],
    drives: Option<&Stage1Invocation>,
    needs_mount_namespace: bool,
) -> Result<()> {
    let started = Instant::now();
    let (enclosures, targets) = read_hiding_plan(
        read_hidden,
        reachable,
        enclosures,
        writable_pins,
        readable_pins,
    );
    if targets.is_empty() && enclosures.is_empty() && drives.is_none() {
        return Ok(());
    }
    if needs_mount_namespace && let Err(error) = enter_private_mount_namespace() {
        bail!("{error:#}; nothing is hidden");
    }
    let fail_after = forced_cover_failure_after();
    let refused_on_purpose = |step: usize| fail_after.is_some_and(|limit| step >= limit);
    let mut covered: Vec<PathBuf> = Vec::new();
    let mut step = 0;
    if let Some(invocation) = drives {
        let covering = if refused_on_purpose(step) {
            Err(anyhow!("refused on purpose by the test helper"))
        } else {
            cover_windows_drives(writable_pins, invocation, &mut covered)
        };
        if let Err(error) = covering {
            return Err(refused_cover(
                Path::new("the Windows drives"),
                error,
                &covered,
            ));
        }
        step += 1;
    }
    for enclosure in &enclosures {
        let covering = if refused_on_purpose(step) {
            Err(anyhow!("refused on purpose by the test helper"))
        } else {
            cover_enclosure(enclosure, writable_pins, readable_pins, &mut covered)
        };
        if let Err(error) = covering {
            return Err(refused_cover(enclosure, error, &covered));
        }
        step += 1;
    }
    for target in &targets {
        let covering = if refused_on_purpose(step) {
            Err(anyhow!("refused on purpose by the test helper"))
        } else {
            hide_contents(target)
        };
        if let Err(error) = covering {
            return Err(refused_cover(target, error, &covered));
        }
        covered.push(target.clone());
        step += 1;
    }
    log::info!(
        "[remote-sandbox] made {} read-hiding mounts in {} ms",
        covered.len(),
        started.elapsed().as_millis()
    );
    Ok(())
}

/// The error for a cover that was refused, once the covers already made have
/// come off again; it says whether they all did.
fn refused_cover(target: &Path, error: anyhow::Error, covered: &[PathBuf]) -> anyhow::Error {
    let stuck = uncover(covered);
    if stuck == 0 {
        anyhow!(
            "covering {}: {error:#}; nothing is hidden",
            target.display()
        )
    } else {
        anyhow!(
            "covering {}: {error:#}; {stuck} covers made before it would not come off, \
             so some entries stay hidden",
            target.display()
        )
    }
}

/// Take the covers in `covered` off again, last first. Returns how many would
/// not come off.
fn uncover(covered: &[PathBuf]) -> usize {
    let mut stuck = 0;
    for target in covered.iter().rev() {
        if let Err(error) = umount2(target, libc::MNT_DETACH) {
            log::warn!(
                "[remote-sandbox] could not take the cover off {}: {error:#}",
                target.display()
            );
            stuck += 1;
        }
    }
    stuck
}

/// For the test helper only: how many covers may succeed before the rest are
/// refused, to exercise [`read_hiding_refused`]. A production build never
/// refuses one on purpose.
fn forced_cover_failure_after() -> Option<usize> {
    #[cfg(feature = "remote-sandbox-test")]
    if let Some(count) = std::env::var_os("ZED_SANDBOX_READ_HIDING_FAIL_AFTER_FOR_TESTS") {
        return count.to_str().and_then(|count| count.parse().ok());
    }
    None
}

/// What it means that the read-hidden entries could not be covered. In WSL
/// mode the mounts are known to work, so the command fails closed. On an SSH
/// host — Ubuntu 24.04 strips the capabilities of an unprivileged user
/// namespace — the sandbox must still be creatable: the command runs without
/// the read hiding (write protection, network isolation and the PID namespace
/// unchanged), and one warning line on stderr, ahead of the command's own
/// output, says so for Zed to show.
fn read_hiding_refused(
    wsl: bool,
    error: anyhow::Error,
) -> std::result::Result<(), (&'static str, anyhow::Error)> {
    if wsl {
        return Err(("mount-failed", error));
    }
    eprintln!("{REMOTE_FAILURE_MARKER}-warning: read-hiding-unavailable: {error:#}");
    Ok(())
}

/// The entries [`hide_read_hidden_entries`] covers: each of `read_hidden` that
/// exists, where it really lies (a dangling symlink has nothing to leak; one
/// that resolves is hidden where it resolves to), split around a root kept
/// inside it (see [`around_reachable`]), once each, and never one that is or
/// holds a root the command must keep reaching.
///
/// What is kept is used exactly as it was approved and pinned — the client's
/// spelling of each root, and the canonical path its pin proved. Resolving
/// those again here is what let one symlink un-hide a credential store: a path
/// that resolved into `~/.ssh` would count as kept, and the cover over `~/.ssh`
/// would be left off for good (see [`reachable_paths`]). What the command is
/// shown beyond its roots is checked against the result instead
/// ([`check_nothing_shown_is_covered`]).
fn read_hidden_targets(
    read_hidden: &[PathBuf],
    reachable: &[PathBuf],
    writable_pins: &[CanonicalPathBuf],
) -> Vec<PathBuf> {
    let mut kept = reachable.to_vec();
    kept.extend(writable_pins.iter().map(|pin| pin.path().to_path_buf()));

    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in read_hidden {
        if let Ok(entry) = std::fs::canonicalize(entry)
            && !entries.contains(&entry)
        {
            entries.push(entry);
        }
    }
    let mut targets: Vec<PathBuf> = Vec::new();
    for candidate in around_reachable(entries, &kept) {
        let Ok(target) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if targets.contains(&target) || kept.iter().any(|path| path.starts_with(&target)) {
            continue;
        }
        targets.push(target);
    }
    targets
}

/// Mount empty content over `target`: an empty read-only `tmpfs` with mode 000
/// for a directory, `/dev/null` (which reads empty) for a file. WSL runs
/// commands as root, which would bypass mode 000, but the command's
/// capabilities are dropped before it runs (see
/// [`finalize_and_exec_command`]); the emptiness hides the contents either way.
fn hide_contents(target: &Path) -> Result<()> {
    if target.is_dir() {
        mount(
            Some(Path::new("tmpfs")),
            target,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_RDONLY,
            Some("mode=000"),
        )
    } else {
        mount(
            Some(Path::new("/dev/null")),
            target,
            None,
            libc::MS_BIND,
            None,
        )
    }
}

/// A private `/dev/shm` too, where POSIX shared memory and semaphores live.
/// Skipped when a writable root is inside it; without one they stay
/// unavailable, as they were before.
fn provide_private_dev_shm(
    writable_pins: &[CanonicalPathBuf],
    temporary_writable: &mut Vec<PathBuf>,
) {
    let Ok(shm) = std::fs::canonicalize("/dev/shm") else {
        return;
    };
    if !shm.is_dir()
        || writable_pins
            .iter()
            .any(|root| root.path().starts_with(&shm))
    {
        return;
    }
    match mount(
        Some(Path::new("tmpfs")),
        &shm,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=1777"),
    ) {
        Ok(()) => temporary_writable.push(shm),
        Err(error) => log::warn!("[remote-sandbox] no private /dev/shm: {error:#}"),
    }
}

fn bind_read_only(pin: &CanonicalPathBuf) -> Result<()> {
    let target = pin.path();
    // `mount` only binds a source from the caller's own mount namespace, and the
    // pin was opened in the parent's, so the path is opened again here — and
    // must still be the pinned inode, so a swap since pinning cannot change what
    // gets mounted.
    let source = crate::util::open_o_path(target, crate::util::FollowSymlinks::Yes)
        .with_context(|| format!("reopening protected {}", target.display()))?;
    let pinned =
        crate::util::linux_fd_identity(pin.fd().as_raw_fd()).context("fstat of pinned inode")?;
    let reopened = crate::util::linux_fd_identity(source.as_raw_fd())
        .context("fstat of reopened protected path")?;
    if pinned != reopened {
        bail!(
            "protected {} was replaced after it was pinned",
            target.display()
        );
    }
    let source_path = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
    mount(
        Some(source_path.as_path()),
        target,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )
    .with_context(|| format!("binding protected {}", target.display()))?;
    remount_read_only(target)
        .with_context(|| format!("remounting protected {} read-only", target.display()))?;
    // `verify_same_inode` uses `lstat`: a target swapped for a symlink would
    // otherwise be followed onto the new bind and pass.
    verify_same_inode(pin, target)
}

/// Remount the bind at `target` read-only, keeping the `nosuid` / `nodev` /
/// `noexec` it inherited. Inside a user namespace those are locked on anything
/// that came from the parent namespace, and a remount dropping one is refused.
fn remount_read_only(target: &Path) -> Result<()> {
    use nix::sys::statvfs::{FsFlags, statvfs};
    let current = statvfs(target)
        .map_err(|errno| anyhow!("statvfs: {errno}"))?
        .flags();
    let mut flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY;
    if current.contains(FsFlags::ST_NOSUID) {
        flags |= libc::MS_NOSUID;
    }
    if current.contains(FsFlags::ST_NODEV) {
        flags |= libc::MS_NODEV;
    }
    if current.contains(FsFlags::ST_NOEXEC) {
        flags |= libc::MS_NOEXEC;
    }
    mount(None, target, None, flags, None)
}

/// Leave the command with no capabilities, and unable to regain any by
/// executing something — even as uid 0 (`SECBIT_NOROOT`).
fn drop_all_capabilities() -> Result<()> {
    const SECBIT_NOROOT: libc::c_ulong = 1 << 0;
    const SECBIT_NOROOT_LOCKED: libc::c_ulong = 1 << 1;
    const SECBIT_NO_SETUID_FIXUP: libc::c_ulong = 1 << 2;
    const SECBIT_NO_SETUID_FIXUP_LOCKED: libc::c_ulong = 1 << 3;
    const SECBIT_KEEP_CAPS_LOCKED: libc::c_ulong = 1 << 5;
    const SECBIT_NO_CAP_AMBIENT_RAISE: libc::c_ulong = 1 << 6;
    const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: libc::c_ulong = 1 << 7;
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    // Above the kernel's own `CAP_LAST_CAP` the drop fails with `EINVAL`, which
    // ends the loop; 63 only bounds it.
    const HIGHEST_CAPABILITY: libc::c_ulong = 63;

    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let securebits = SECBIT_NOROOT
        | SECBIT_NOROOT_LOCKED
        | SECBIT_NO_SETUID_FIXUP
        | SECBIT_NO_SETUID_FIXUP_LOCKED
        | SECBIT_KEEP_CAPS_LOCKED
        | SECBIT_NO_CAP_AMBIENT_RAISE
        | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
    let unused: libc::c_ulong = 0;
    // SAFETY: `prctl` with scalar arguments only.
    if unsafe { libc::prctl(libc::PR_SET_SECUREBITS, securebits, unused, unused, unused) } != 0 {
        bail!(
            "prctl(PR_SET_SECUREBITS): {}",
            std::io::Error::last_os_error()
        );
    }
    // The bounding set goes before `capset`, which gives up the `CAP_SETPCAP`
    // this needs.
    for capability in 0..=HIGHEST_CAPABILITY {
        // SAFETY: `prctl` with scalar arguments only.
        if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, unused, unused, unused) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            bail!("prctl(PR_CAPBSET_DROP, {capability}): {error}");
        }
    }
    let clear_all = libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong;
    // SAFETY: `prctl` with scalar arguments only.
    if unsafe { libc::prctl(libc::PR_CAP_AMBIENT, clear_all, unused, unused, unused) } != 0 {
        bail!(
            "prctl(PR_CAP_AMBIENT_CLEAR_ALL): {}",
            std::io::Error::last_os_error()
        );
    }
    let header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // SAFETY: `header` and the two-element `data` array have the layout
    // `capset(2)` expects for `_LINUX_CAPABILITY_VERSION_3`, and outlive the call.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_capset,
            &header as *const CapabilityHeader,
            data.as_ptr(),
        )
    };
    if ret != 0 {
        bail!("capset: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// `--wsl-sandbox-resolve-path <path>`: print the WSL distro name, the canonical
/// (symlink-free) target of `path`, and whether that target lives on a Windows
/// drive, one per line (read by `windows_wsl::resolve_canonical_for_grant`).
fn run_wsl_resolve(args: &[OsString]) -> ! {
    let [path] = args else {
        eprintln!("zed: malformed WSL canonical-path resolver invocation");
        std::process::exit(127);
    };
    let canonical = match CanonicalPathBuf::resolve(PathBuf::from(path)) {
        Ok(canonical) => canonical,
        Err(error) => {
            eprintln!("zed: could not resolve WSL sandbox grant: {error}");
            std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
        }
    };
    let Some(canonical_text) = canonical.path().to_str() else {
        eprintln!("zed: canonical WSL sandbox grant is not valid UTF-8");
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    };
    let Ok(distro) = std::env::var("WSL_DISTRO_NAME") else {
        eprintln!("zed: WSL_DISTRO_NAME is unavailable while resolving sandbox grant");
        std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
    };
    // Classified on the canonical target (what will actually be granted), so
    // the Windows side can warn about a grant on a Windows drive.
    let filesystem_class = if path_is_on_windows_fs(canonical.path()) {
        "windows-fs"
    } else {
        "native-fs"
    };
    println!("{distro}");
    println!("{canonical_text}");
    println!("{filesystem_class}");
    std::process::exit(0);
}

/// Whether `path` resides on a Windows drive exposed to WSL via DrvFs (9p or
/// virtiofs), as opposed to the distro's own filesystems. Decided on the
/// filesystem type rather than a `/mnt/` prefix, so a moved automount root or a
/// symlink onto a Windows drive is still recognized; an unknown type reports
/// `true`, which only adds a warning.
fn path_is_on_windows_fs(path: &Path) -> bool {
    const V9FS_MAGIC: u64 = 0x0102_1997;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;
    match nix::sys::statfs::statfs(path) {
        Ok(stat) => {
            // `f_type` is `i64` on glibc but `u64` on musl, so widen it
            // losslessly instead of comparing against a fixed-width constant.
            #[allow(
                clippy::useless_conversion,
                reason = "the conversion is a real widen on glibc (i64) and a no-op on musl (u64); \
                          clippy only sees the musl case when cross-compiling the remote server"
            )]
            u64::try_from(stat.filesystem_type().0)
                .is_ok_and(|fs_type| fs_type == V9FS_MAGIC || fs_type == FUSE_SUPER_MAGIC)
        }
        Err(_) => true,
    }
}

/// `--wsl-sandbox-probe`: report on one [`WSL_PROBE_RESULT_PREFIX`] line the
/// Landlock ABI and whether a user + network namespace can be created — the
/// two things a WSL kernel can lack — so that the Windows side can explain the
/// problem before any command is run.
fn run_wsl_probe() -> ! {
    let namespaces = match probe_user_and_network_namespaces() {
        Ok(()) => "ok".to_string(),
        Err(error) => error,
    };
    println!(
        "{WSL_PROBE_RESULT_PREFIX} landlock={} userns={namespaces}",
        landlock_abi()
    );
    std::process::exit(0);
}

/// Try the holder's `unshare(CLONE_NEWUSER | CLONE_NEWNET)` in a throwaway
/// child. The error is a single word (`errno-<n>`), since it goes on the probe's
/// space-separated result line.
fn probe_user_and_network_namespaces() -> std::result::Result<(), String> {
    // SAFETY: this runs at the top of `main`, before the process starts any
    // thread; the child only calls the async-signal-safe `unshare` and `_exit`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!(
            "errno-{}",
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        ));
    }
    if pid == 0 {
        // SAFETY: see above.
        let failed = unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0;
        let code = if failed {
            std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(1)
                .clamp(1, 255)
        } else {
            0
        };
        // SAFETY: `_exit` is async-signal-safe and never returns.
        unsafe { libc::_exit(code) };
    }
    match wait_pid(pid) {
        Ok(0) => Ok(()),
        Ok(code) => Err(format!("errno-{code}")),
        Err(_) => Err("wait-failed".to_string()),
    }
}

// ===========================================================================
// Mount jail (older kernels without Landlock, e.g. 18.04 / kernel 5.4)
// ===========================================================================

/// Build a read-only view of `/` with a read-write worktree (and protected
/// paths re-bound read-only over it), then `pivot_root` into it and detach the
/// old root. Requires a fresh mount namespace, which we can create because we
/// are inside our own user namespace.
fn apply_mount_jail(
    writable_pins: &[CanonicalPathBuf],
    protected_pins: &[CanonicalPathBuf],
    allow_fs_write: bool,
    scratch_pid: libc::pid_t,
) -> Result<()> {
    // Fresh mount namespace (allowed inside our userns).
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        bail!(
            "unshare(CLONE_NEWNS) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    // Don't propagate our mounts back to the host.
    mount(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .context("making / private")?;

    let new_root = PathBuf::from(format!("/tmp/.zed-remote-jail-{scratch_pid}"));
    remove_owned_temp_dir(&new_root);
    std::fs::create_dir_all(&new_root)
        .with_context(|| format!("creating {}", new_root.display()))?;

    // Bind the whole tree onto new_root...
    mount(
        Some(Path::new("/")),
        &new_root,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )
    .context("bind-mounting / onto the new root")?;
    if !allow_fs_write {
        // ...and remount it read-only. On these kernels the recursive-RO remount
        // is not a single op, so this makes the top bind RO; the writable
        // exceptions are re-bound below and the sensitive dirs on typical
        // single-filesystem hosts are covered.
        mount(
            None,
            &new_root,
            None,
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            None,
        )
        .context("remounting the new root read-only")?;
    }

    // Private tmpfs at /tmp inside the jail — mounted *before* the worktree binds
    // so that a worktree living under `/tmp` is re-bound *on top of* the fresh
    // tmpfs (rather than shadowed by it). The bind sources below still name the
    // real paths, which stay visible in this mount namespace.
    let jail_tmp = new_root.join("tmp");
    // `/tmp` normally exists already; if it cannot, the mount below says so.
    if let Err(error) = std::fs::create_dir_all(&jail_tmp) {
        log::debug!("[remote-sandbox] creating {}: {error}", jail_tmp.display());
    }
    mount(Some(Path::new("tmpfs")), &jail_tmp, Some("tmpfs"), 0, None)
        .context("mounting private /tmp")?;

    // Re-bind the writable paths read-write and the protected ones read-only
    // over the read-only root, **outermost first** and, at the same depth, the
    // protection last. Each mount lands on top of the ones before it, so a
    // protected entry inside a writable root stays read-only — and a writable
    // root inside a protected entry stays writable instead of being shadowed by
    // the protection of what holds it, which is how a sandboxed agent writes
    // its own configuration directory inside the agents' directory (ADR-F49
    // decision 15) on the kernels that use this backend.
    let mut binds: Vec<(&CanonicalPathBuf, bool)> = writable_pins
        .iter()
        .map(|pin| (pin, false))
        .chain(protected_pins.iter().map(|pin| (pin, true)))
        .collect();
    binds.sort_by_key(|(pin, read_only)| (pin.path().components().count(), *read_only));
    for (pin, read_only) in binds {
        let target = join_absolute(&new_root, pin.path());
        if read_only {
            if !target.exists() {
                continue;
            }
        } else if !ensure_bind_target(&target) {
            continue;
        }
        let what = if read_only { "ro" } else { "rw" };
        mount(
            Some(pin.path()),
            &target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .with_context(|| format!("{what}-binding {}", pin.path().display()))?;
        if read_only {
            mount(
                None,
                &target,
                None,
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
                None,
            )
            .with_context(|| format!("remounting protected {} read-only", pin.path().display()))?;
        } else {
            // Verify the bound inode is the one we pinned (TOCTOU backstop).
            verify_same_inode(pin, &target)
                .with_context(|| format!("verifying rw bind of {}", pin.path().display()))?;
        }
    }

    // pivot_root into new_root, putting the old root under /tmp/.oldroot.
    let put_old = jail_tmp.join(".oldroot");
    std::fs::create_dir_all(&put_old).with_context(|| format!("creating {}", put_old.display()))?;
    pivot_root(&new_root, &put_old).context("pivot_root")?;
    std::env::set_current_dir("/").context("chdir / after pivot_root")?;
    // Detach the old root so nothing outside the jail is reachable.
    umount2(Path::new("/tmp/.oldroot"), libc::MNT_DETACH).context("detaching old root")?;
    // Only an empty directory in the jail's own tmpfs, gone with the command.
    if let Err(error) = std::fs::remove_dir("/tmp/.oldroot") {
        log::debug!("[remote-sandbox] leaving /tmp/.oldroot: {error}");
    }
    Ok(())
}

/// Join an absolute `path` under `base` (dropping `path`'s leading `/`).
fn join_absolute(base: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix("/").unwrap_or(path);
    base.join(relative)
}

/// Ensure `target` exists as a bind mount point. Under the read-only root the
/// real worktree's target already exists (so this is a no-op); the creation path
/// only matters for a worktree living under the writable tmpfs `/tmp`. Returns
/// false (skip the bind) if the target is missing and can't be created.
fn ensure_bind_target(target: &Path) -> bool {
    if target.exists() {
        return true;
    }
    match std::fs::create_dir_all(target) {
        Ok(()) => true,
        Err(error) => {
            log::warn!(
                "[remote-sandbox] could not create bind target {}: {error}",
                target.display()
            );
            false
        }
    }
}

fn verify_same_inode(pin: &CanonicalPathBuf, mounted: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let mounted_meta = std::fs::symlink_metadata(mounted)
        .with_context(|| format!("lstat {}", mounted.display()))?;
    let expected =
        crate::util::linux_fd_identity(pin.fd().as_raw_fd()).context("fstat of pinned inode")?;
    let actual = (mounted_meta.dev(), mounted_meta.ino());
    if expected != actual {
        bail!(
            "bind of {} was redirected (pinned {:?}, mounted {:?})",
            mounted.display(),
            expected,
            actual
        );
    }
    Ok(())
}

fn mount(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let source = source.map(path_to_cstring).transpose()?;
    let target = path_to_cstring(target)?;
    let fstype = fstype.map(|s| CString::new(s)).transpose()?;
    let data = data.map(|s| CString::new(s)).transpose()?;
    let source_ptr = source.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let fstype_ptr = fstype.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    let data_ptr = data
        .as_ref()
        .map_or(std::ptr::null(), |c| c.as_ptr().cast());
    // SAFETY: all pointers are valid C strings (or NULL) living across the call.
    let ret = unsafe { libc::mount(source_ptr, target.as_ptr(), fstype_ptr, flags, data_ptr) };
    if ret != 0 {
        bail!(
            "mount({}) failed: {}",
            target.to_string_lossy(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn umount2(target: &Path, flags: libc::c_int) -> Result<()> {
    let target = path_to_cstring(target)?;
    // SAFETY: `target` is a valid C string living across the call.
    let ret = unsafe { libc::umount2(target.as_ptr(), flags) };
    if ret != 0 {
        bail!("umount2 failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn pivot_root(new_root: &Path, put_old: &Path) -> Result<()> {
    let new_root = path_to_cstring(new_root)?;
    let put_old = path_to_cstring(put_old)?;
    // SAFETY: both are valid C strings; `SYS_pivot_root` takes two path args.
    let ret = unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) };
    if ret != 0 {
        bail!("pivot_root failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn path_to_cstring(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("path {} contains a NUL byte", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_remote_sandbox_helper_argv;
    use crate::protected_paths::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE;
    use crate::protected_paths::remote_builtin_protected_paths;
    use http_proxy::DestinationPolicy as _;

    /// ADR-F49 decision 11 on WSL: which roots are refused for bringing a
    /// Windows home's credentials back with them — a home, anything above one,
    /// or a folder in one holding its entries — and which starting directories
    /// stay covered for lying in them, however they are spelled.
    #[test]
    fn windows_homes_are_recognised_on_the_drives() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("mnt");
        let drives = vec![root.join("c"), root.join("d")];
        let home = root.join("c").join("Users").join("u");
        let relocated = root.join("d").join("profiles").join("u");
        let not_a_home = root.join("d").join("backup");
        for directory in [
            home.join("AppData").join("Local").join("x"),
            home.join(".ssh"),
            home.join(".config").join("gh"),
            home.join(".config").join("project"),
            home.join(".docker"),
            home.join("projects").join("p"),
            relocated.join("AppData").join("Roaming"),
            relocated.join("projects"),
            not_a_home.clone(),
        ] {
            std::fs::create_dir_all(&directory).expect("create");
        }
        std::fs::write(relocated.join("NTUSER.DAT"), "").expect("write");
        std::fs::write(not_a_home.join("NTUSER.DAT"), "").expect("write");
        std::os::unix::fs::symlink(home.join("AppData"), home.join("Application Data"))
            .expect("symlink");
        std::os::unix::fs::symlink(home.join(".config"), home.join("config-link"))
            .expect("symlink");

        for refused in [
            temporary.path().to_path_buf(),
            root.clone(),
            root.join("c"),
            root.join("c").join("users"),
            home.clone(),
            relocated.clone(),
            home.join(".config"),
            home.join("config-link"),
            home.join(".docker"),
        ] {
            assert!(
                windows_home_exposure(&refused, &drives).is_some(),
                "{}",
                refused.display()
            );
        }
        for allowed in [
            home.join("projects").join("p"),
            home.join(".config").join("project"),
            home.join(".ssh"),
            relocated.join("projects"),
            root.join("d"),
            not_a_home,
        ] {
            assert!(
                windows_home_exposure(&allowed, &drives).is_none(),
                "{}",
                allowed.display()
            );
        }

        for hidden in [
            home.join("AppData").join("Local").join("x"),
            home.join("Application Data").join("Local"),
            home.join(".ssh"),
            relocated.join("AppData").join("Roaming"),
        ] {
            assert!(
                inside_hidden_windows_entry(&hidden, &drives),
                "{}",
                hidden.display()
            );
        }
        for project in [home.join("projects").join("p"), relocated.join("projects")] {
            assert!(
                !inside_hidden_windows_entry(&project, &drives),
                "{}",
                project.display()
            );
        }
    }

    /// The Windows drives are found by filesystem type wherever they are
    /// mounted, and WSL's driver store, also over 9p, is not one of them.
    #[test]
    fn drives_are_found_in_mountinfo_by_filesystem_type() {
        let mountinfo = "\
610 1 8:48 / / rw,relatime - ext4 /dev/sdd rw,discard,errors=remount-ro
689 610 0:36 / /usr/lib/wsl/drivers ro,nosuid,nodev,noatime - 9p drivers ro,aname=drivers;fmask=222;dmask=222,cache=0x5
699 610 0:97 / /mnt/c rw,noatime - 9p C:\\134 rw,aname=drvfs;path=C:\\;uid=0;gid=0;symlinkroot=/mnt/,cache=0x5
700 610 0:98 / /d rw,noatime shared:5 master:1 - 9p D:\\134 rw,aname=drvfs;path=D:\\;uid=0;gid=0
701 610 0:99 / /mnt/my\\040drive rw,noatime - drvfs X:\\134 rw
702 610 0:100 / /e rw,noatime - virtiofs drvfsE rw
703 610 0:30 / /mnt/wsl rw,relatime shared:1 - tmpfs none rw
";
        assert_eq!(
            drive_mount_points(mountinfo),
            [
                PathBuf::from("/d"),
                PathBuf::from("/e"),
                PathBuf::from("/mnt/c"),
                PathBuf::from("/mnt/my drive"),
            ]
        );
    }

    /// A virtiofs share is a drive only under WSL's drive tag: the driver
    /// store and GPU libraries, shared over virtiofs too, stay uncovered.
    #[test]
    fn only_drvfs_tagged_virtiofs_shares_are_drives() {
        let mountinfo = "\
702 610 0:100 / /mnt/e rw,noatime - virtiofs drvfsE rw
704 610 0:102 / /usr/lib/wsl/drivers ro,nosuid,nodev,noatime - virtiofs drivers ro
705 610 0:103 / /usr/lib/wsl/lib ro,nosuid,nodev,noatime - virtiofs gpu_lib ro
";
        assert_eq!(drive_mount_points(mountinfo), [PathBuf::from("/mnt/e")]);
    }

    /// Round-trip the builder through the stage-1 decoder to lock the argv
    /// protocol both sides share.
    #[test]
    fn stage1_decode_round_trips_the_builder() {
        let mut argv = build_remote_sandbox_helper_argv(
            &["/home/u/proj".to_string(), "/home/u/scratch".to_string()],
            &["/home/u/proj/.git".to_string()],
            &["/home/u/.local/share/zed-jp-enhance/node".to_string()],
            &RemoteSandboxNetwork::Hosts(vec!["example.com".into()]),
            false,
            Some("/home/u/proj/sub"),
        );
        argv.push("/bin/bash".to_string());
        argv.push("-c".to_string());
        argv.push("echo hi".to_string());

        // `parse_stage1_args` takes the arguments *after* the helper flag.
        assert_eq!(argv[0], REMOTE_SANDBOX_HELPER_FLAG);
        let decoded = decode_for_test(&argv[1..]);

        assert_eq!(
            decoded.network,
            RemoteSandboxNetwork::Hosts(vec!["example.com".into()])
        );
        assert!(!decoded.allow_fs_write);
        assert!(!decoded.wsl, "the SSH builder must not select WSL mode");
        assert!(
            decoded.network_ceiling.is_none(),
            "an SSH host gets no editor-wide allowlist"
        );
        assert!(decoded.set_environment.is_empty());
        assert_eq!(
            decoded.writable_paths,
            vec![
                PathBuf::from("/home/u/proj"),
                PathBuf::from("/home/u/scratch")
            ]
        );
        assert_eq!(
            decoded.protected_paths,
            vec![PathBuf::from("/home/u/proj/.git")]
        );
        assert_eq!(
            decoded.readable_paths,
            vec![PathBuf::from("/home/u/.local/share/zed-jp-enhance/node")]
        );
        assert_eq!(decoded.chdir, Some(PathBuf::from("/home/u/proj/sub")));
        assert_eq!(decoded.program, OsString::from("/bin/bash"));
        assert_eq!(
            decoded.args,
            vec![OsString::from("-c"), OsString::from("echo hi")]
        );
    }

    fn decode_for_test(args: &[String]) -> Stage1Invocation {
        parse_stage1_args(args.iter().map(OsString::from)).expect("the builder's argv must parse")
    }

    /// ADR-F46 C-1: the command's environment is built from nothing, but `HOME`
    /// / `USER` / `LOGNAME` are filled in from the account database — a remote
    /// client cannot send them (it does not know the account's home) and `git`
    /// / `npm` / `cargo` break without `HOME`. A value the client sends
    /// explicitly wins over the account database, and the default `PATH` is
    /// there.
    #[test]
    fn the_command_environment_fills_home_and_user_from_the_account() {
        let account = account_from_passwd().expect("this account has a passwd entry");
        let tmpdir = PathBuf::from("/tmp/zed-remote-sandbox-1");

        let filled_in = command_environment(&invocation_writing(&[]), &tmpdir, None);
        let value = |name: &str| {
            filled_in
                .iter()
                .find(|(existing, _)| existing == OsStr::new(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("HOME"), Some(account.home.clone().into_os_string()));
        assert_eq!(value("PATH"), Some(OsString::from(DEFAULT_PATH)));
        if account.name.is_empty() {
            assert_eq!(value("USER"), None);
            assert_eq!(value("LOGNAME"), None);
        } else {
            assert_eq!(value("USER"), Some(account.name.clone()));
            assert_eq!(value("LOGNAME"), Some(account.name));
        }

        // What the client sends explicitly wins over the account database.
        let mut invocation = invocation_writing(&[]);
        invocation.set_environment = vec![
            (OsString::from("HOME"), OsString::from("/somewhere/else")),
            (OsString::from("USER"), OsString::from("someone")),
        ];
        let overridden = command_environment(&invocation, &tmpdir, None);
        let value = |name: &str| {
            overridden
                .iter()
                .find(|(existing, _)| existing == OsStr::new(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("HOME"), Some(OsString::from("/somewhere/else")));
        assert_eq!(value("USER"), Some(OsString::from("someone")));
        // And each of them appears exactly once — the account value is
        // overwritten, not left beside the client's.
        for name in ["HOME", "USER"] {
            assert_eq!(
                overridden
                    .iter()
                    .filter(|(existing, _)| existing == OsStr::new(name))
                    .count(),
                1,
                "{name} appears once"
            );
        }
    }

    /// The WSL backend's argv selects WSL mode, carries the environment, and
    /// still decodes the shared policy flags exactly like the SSH builder's.
    #[test]
    fn wsl_argv_round_trips_mode_and_environment() {
        let ceiling = ["api.github.com".to_string(), "10.0.0.5".to_string()];
        let environment = [
            ("PAGER".to_string(), String::new()),
            ("GREETING".to_string(), "a=b -- --wsl".to_string()),
        ];
        let mut argv = crate::build_wsl_sandbox_helper_argv(
            &["/mnt/c/proj".to_string()],
            &["/mnt/c/proj/.git".to_string()],
            &[],
            &RemoteSandboxNetwork::AnyHost,
            false,
            Some("/mnt/c/proj"),
            &crate::WslHelperOptions {
                network_ceiling: Some(&ceiling),
                slirp_sha256: Some("abc123"),
                environment: &environment,
                unset_environment: &["WSL_INTEROP", "WSLENV"],
                interactive_session: None,
            },
        );
        argv.push("/bin/sh".to_string());
        argv.push("-c".to_string());
        argv.push("--wsl".to_string());

        assert_eq!(argv[0], REMOTE_SANDBOX_HELPER_FLAG);
        let decoded = decode_for_test(&argv[1..]);
        assert!(decoded.wsl);
        assert_eq!(decoded.network, RemoteSandboxNetwork::AnyHost);
        let ceiling = decoded
            .network_ceiling
            .as_ref()
            .expect("the WSL argv carries the editor-wide allowlist");
        for (host, allowed) in [
            ("api.github.com", true),
            ("10.0.0.5", true),
            ("localhost", true),
            ("github.com", false),
        ] {
            assert_eq!(
                matches!(
                    ceiling.evaluate(host, 443),
                    http_proxy::PolicyDecision::Allow { .. }
                ),
                allowed,
                "{host}"
            );
        }
        assert_eq!(decoded.slirp_sha256.as_deref(), Some("abc123"));
        assert_eq!(decoded.writable_paths, vec![PathBuf::from("/mnt/c/proj")]);
        assert_eq!(
            decoded.protected_paths,
            vec![PathBuf::from("/mnt/c/proj/.git")]
        );
        assert_eq!(
            decoded.set_environment,
            vec![
                (OsString::from("PAGER"), OsString::new()),
                (OsString::from("GREETING"), OsString::from("a=b -- --wsl")),
            ]
        );
        assert_eq!(
            decoded.unset_environment,
            vec![OsString::from("WSL_INTEROP"), OsString::from("WSLENV")]
        );
        assert_eq!(decoded.program, OsString::from("/bin/sh"));
        assert_eq!(
            decoded.args,
            vec![OsString::from("-c"), OsString::from("--wsl")]
        );
    }

    /// Only a `--wsl` among the policy flags selects the WSL failure marker,
    /// not one that is part of the command after `--`.
    #[test]
    fn wsl_mode_is_read_from_the_policy_flags_only() {
        let policy = |args: &[&str]| args.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(requests_wsl_mode(&policy(&[
            "--net", "blocked", "--wsl", "--", "sh"
        ])));
        assert!(!requests_wsl_mode(&policy(&[
            "--net", "blocked", "--", "--wsl"
        ])));
        assert!(!requests_wsl_mode(&policy(&[
            "--net", "blocked", "--", "sh"
        ])));
    }

    /// `--network-ceiling` on its own still installs an allowlist (an empty one,
    /// leaving loopback), and an entry that doesn't parse fails the invocation
    /// instead of being dropped.
    #[test]
    fn ceiling_flags_decode_and_malformed_entries_fail() {
        let parse = |args: &[&str]| parse_stage1_args(args.iter().map(OsString::from));
        let empty = parse(&["--net", "any", "--network-ceiling", "--", "true"])
            .expect("an empty allowlist parses");
        let ceiling = empty.network_ceiling.expect("an allowlist is installed");
        assert!(matches!(
            ceiling.evaluate("example.com", 443),
            http_proxy::PolicyDecision::Deny(_)
        ));
        assert!(matches!(
            ceiling.evaluate("127.0.0.1", 443),
            http_proxy::PolicyDecision::Allow { .. }
        ));
        assert!(
            parse(&["--net", "any", "--ceiling-host", "foo.*.com", "--", "true"]).is_err(),
            "a malformed entry must fail closed"
        );
        assert!(
            parse(&["--net", "any", "--", "true"])
                .expect("no allowlist parses")
                .network_ceiling
                .is_none()
        );
    }

    /// The first response line and headers a proxy gives a CONNECT to `target`.
    fn connect_response(proxy_port: u16, target: &str) -> String {
        use std::io::{BufRead as _, BufReader, Write as _};
        let mut stream =
            std::net::TcpStream::connect(("127.0.0.1", proxy_port)).expect("connect to the proxy");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("set a read timeout");
        write!(
            stream,
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
        )
        .expect("send the CONNECT");
        let mut reader = BufReader::new(stream);
        let mut response = String::new();
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read the response");
            if read == 0 || line == "\r\n" {
                break;
            }
            response.push_str(&line);
        }
        response
    }

    /// The helper's proxy nests the command's hosts inside the editor-wide
    /// allowlist: refused by the allowlist even for an any-host grant, while
    /// loopback stays reachable under it as it does on macOS and Linux.
    #[test]
    fn the_egress_proxy_is_nested_inside_the_editor_allowlist() {
        let local_service =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a local service");
        let local_target = format!(
            "127.0.0.1:{}",
            local_service.local_addr().expect("local address").port()
        );
        let ceiling = Arc::new(
            crate::EditorAllowlistCeiling::parse(&["example.com".to_string()])
                .expect("valid allowlist"),
        );

        let proxy = start_egress_proxy(&RemoteSandboxNetwork::AnyHost, Some(&ceiling))
            .expect("start the proxy");
        let refused = connect_response(proxy.port(), "evil.example:443");
        assert!(refused.starts_with("HTTP/1.1 511"), "{refused}");
        assert!(refused.contains("Via: 1.1 zed-egress-proxy"), "{refused}");
        let local = connect_response(proxy.port(), &local_target);
        assert!(local.starts_with("HTTP/1.1 200"), "{local}");

        let proxy = start_egress_proxy(
            &RemoteSandboxNetwork::Hosts(vec!["evil.example".to_string()]),
            Some(&ceiling),
        )
        .expect("start the proxy");
        let refused = connect_response(proxy.port(), "evil.example:443");
        assert!(refused.contains("Via: 1.1 zed-egress-proxy"), "{refused}");
    }

    /// Without an editor-wide allowlist — always so on an SSH host — an
    /// any-host grant reaches public addresses only, however the helper was
    /// started: the host's own services, its LAN, the metadata service and
    /// every other local address are refused, as is a name resolving there.
    #[test]
    fn an_uncapped_any_host_grant_refuses_local_addresses() {
        let local_service =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a local service");
        let port = local_service.local_addr().expect("local address").port();

        let proxy =
            start_egress_proxy(&RemoteSandboxNetwork::AnyHost, None).expect("start the proxy");
        for target in [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
            format!("[::ffff:127.0.0.1]:{port}"),
            "127.0.0.2:22".to_string(),
            "10.1.2.3:443".to_string(),
            "172.16.0.1:443".to_string(),
            "192.168.100.150:22".to_string(),
            "169.254.169.254:80".to_string(),
            "100.64.0.1:443".to_string(),
            "0.0.0.0:22".to_string(),
            "[fe80::1]:22".to_string(),
            "[fd00::1]:22".to_string(),
        ] {
            let response = connect_response(proxy.port(), &target);
            assert!(response.starts_with("HTTP/1.1 511"), "{target}: {response}");
        }
    }

    /// The limit to public addresses replaces the allowlist's own decision
    /// only for an uncapped any-host grant, and lets every public host
    /// through; a host list and an editor-wide allowlist keep deciding for
    /// themselves.
    #[test]
    fn only_an_uncapped_any_host_grant_is_limited_to_public_addresses() {
        let policy = any_host_policy(&RemoteSandboxNetwork::AnyHost, None)
            .expect("an uncapped any-host grant is limited");
        for host in [
            "example.com",
            "93.184.215.14",
            "2606:2800:21f:cb07:6820:80da:af6b:8b2c",
        ] {
            assert!(
                matches!(
                    policy.evaluate(host, 443),
                    http_proxy::PolicyDecision::Allow {
                        permit_local_addresses: false
                    }
                ),
                "{host}"
            );
        }
        let ceiling = Arc::new(
            crate::EditorAllowlistCeiling::parse(&["example.com".to_string()])
                .expect("valid allowlist"),
        );
        assert!(any_host_policy(&RemoteSandboxNetwork::AnyHost, Some(&ceiling)).is_none());
        assert!(
            any_host_policy(
                &RemoteSandboxNetwork::Hosts(vec!["example.com".to_string()]),
                None
            )
            .is_none()
        );
        assert!(any_host_policy(&RemoteSandboxNetwork::Blocked, None).is_none());
    }

    /// ADR-F49: every address this host holds is out of reach under an uncapped
    /// any-host grant, whatever range it is in. The proxy runs in the host's own
    /// network namespace, so its *public* addresses reach whatever is bound to
    /// `0.0.0.0` or `::` there — its sshd, a local model server — and no list of
    /// host patterns can name them. A public address that is not this host's
    /// stays reachable.
    #[test]
    fn an_uncapped_any_host_grant_refuses_this_host_s_own_addresses() {
        let policy = any_host_policy(&RemoteSandboxNetwork::AnyHost, None)
            .expect("an uncapped any-host grant is limited");
        let mut own = 0;
        for interface in nix::ifaddrs::getifaddrs().expect("this host's own addresses") {
            let Some(storage) = interface.address else {
                continue;
            };
            let address = match (storage.as_sockaddr_in(), storage.as_sockaddr_in6()) {
                (Some(v4), _) => std::net::IpAddr::V4(v4.ip()),
                (_, Some(v6)) => std::net::IpAddr::V6(v6.ip()),
                _ => continue,
            };
            assert!(
                policy.forbids_address(address),
                "{address} is one of this host's own"
            );
            own += 1;
        }
        assert!(own > 0, "this host holds at least its loopback address");
        assert!(
            !policy.forbids_address("93.184.215.14".parse().expect("an address")),
            "a public address that is not this host's stays reachable"
        );
    }

    /// ADR-F49 decision 27 (final review, F-2): an address of this host reached
    /// through an embedded-IPv4 encoding other than IPv4-mapped — NAT64
    /// (`64:ff9b::/96`), 6to4 (`2002::/16`) or IPv4-compatible (`::/96`) — is
    /// refused too, so a `CONNECT` cannot reach the host's own public IPv4
    /// service by dressing it as an ordinary IPv6 address.
    #[test]
    fn an_uncapped_any_host_grant_refuses_embedded_ipv4_forms_of_this_host() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        let policy = any_host_policy(&RemoteSandboxNetwork::AnyHost, None)
            .expect("an uncapped any-host grant is limited");
        let embedded_forms = |v4: Ipv4Addr| {
            let value = u32::from(v4);
            let high = (value >> 16) as u16;
            let low = (value & 0xffff) as u16;
            [
                Ipv6Addr::new(0, 0, 0, 0, 0, 0, high, low), // IPv4-compatible ::/96
                Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, high, low), // NAT64 64:ff9b::/96
                Ipv6Addr::new(0x2002, high, low, 0, 0, 0, 0, 0), // 6to4 2002::/16
            ]
        };

        let mut own_ipv4 = 0;
        for interface in nix::ifaddrs::getifaddrs().expect("this host's own addresses") {
            let Some(storage) = interface.address else {
                continue;
            };
            let Some(v4) = storage.as_sockaddr_in() else {
                continue;
            };
            let v4 = v4.ip();
            for form in embedded_forms(v4) {
                assert!(
                    policy.forbids_address(IpAddr::V6(form)),
                    "the embedded-IPv4 form {form} of this host's own {v4} must be refused"
                );
            }
            own_ipv4 += 1;
        }
        assert!(
            own_ipv4 > 0,
            "this host holds at least its loopback IPv4 address"
        );
    }

    /// A name `setenv(3)` rejects is a malformed invocation, caught at parse time
    /// rather than as a panic in the holder.
    #[test]
    fn environment_names_setenv_would_reject_are_refused() {
        for name in ["", "A=B"] {
            let args = ["--setenv", name, "value", "--", "true"];
            assert!(
                parse_stage1_args(args.iter().map(OsString::from)).is_err(),
                "{name:?} must be rejected"
            );
        }
        let args = ["--unsetenv", "=C:", "--", "true"];
        assert!(parse_stage1_args(args.iter().map(OsString::from)).is_err());
    }

    /// In WSL mode a writable root must still be exactly the canonical path the
    /// Windows side approved: one reached through a symlink fails closed, one
    /// that is gone is simply not granted.
    #[test]
    fn strict_writable_pins_refuse_a_redirected_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let real = root.path().join("real");
        std::fs::create_dir(&real).expect("create real");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let canonical_real = pin_path(&real).expect("pin real").path().to_path_buf();

        let pins = pin_writable_paths(std::slice::from_ref(&canonical_real), true)
            .expect("a canonical root pins");
        assert_eq!(pins.len(), 1);

        assert!(
            pin_writable_paths(std::slice::from_ref(&link), true).is_err(),
            "a root reached through a symlink must fail closed in WSL mode"
        );
        assert_eq!(
            pin_writable_paths(&[link], false)
                .expect("the SSH mode follows it")
                .len(),
            1
        );

        let gone = root.path().join("gone");
        assert!(
            pin_writable_paths(&[gone], true)
                .expect("a missing root is skipped")
                .is_empty()
        );
    }

    /// Each protected path is classified by what is there and located where the
    /// entry itself lives: the link for a symlink, and for a missing entry where
    /// it would be created, below its deepest existing ancestor.
    #[test]
    fn protected_entries_are_classified_by_what_is_there() {
        let (_root, root) = canonical_tempdir();
        let directory = root.join(".git");
        std::fs::create_dir(&directory).expect("create .git");
        let target = root.join("dotfiles-bashrc");
        std::fs::write(&target, "x").expect("write the link target");
        let link = root.join(".bashrc");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let missing = root.join(".bash_login");
        let missing_below_missing = root.join(".config/fish/config.fish");

        let entries = classify_protected_paths(&[
            directory.clone(),
            link.clone(),
            missing.clone(),
            missing_below_missing.clone(),
            target.join("below-a-file"),
        ])
        .expect("classify");
        let classified: Vec<(&Path, &str)> = entries
            .iter()
            .map(|entry| {
                let kind = match entry.kind {
                    ProtectedKind::Present(_) => "present",
                    ProtectedKind::Symlink(_) => "symlink",
                    ProtectedKind::Missing => "missing",
                };
                (entry.location.as_path(), kind)
            })
            .collect();
        assert_eq!(
            classified,
            [
                (directory.as_path(), "present"),
                (link.as_path(), "symlink"),
                (missing.as_path(), "missing"),
                (missing_below_missing.as_path(), "missing"),
            ],
            "a path below a plain file can never exist and is dropped"
        );
        let symlink = entries.get(1).expect("the symlink entry");
        assert_eq!(
            symlink.carve_targets().collect::<Vec<_>>(),
            [link.as_path(), target.as_path()]
        );
    }

    /// Create a Landlock ruleset for a test. Creating one only allocates an fd;
    /// nothing is enforced until `landlock_restrict_self`, which no test calls
    /// (it is irreversible and would restrict the whole test process).
    fn create_test_ruleset(abi: i32) -> OwnedFd {
        let attr = LandlockRulesetAttr {
            handled_access_fs: landlock_handled_write_access(abi),
            handled_access_net: 0,
            scoped: 0,
        };
        // SAFETY: `attr` is a valid, correctly-sized ruleset attr; flags 0.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const LandlockRulesetAttr,
                std::mem::size_of::<LandlockRulesetAttr>(),
                0u32,
            )
        };
        assert!(fd >= 0, "landlock_create_ruleset failed in a test");
        // SAFETY: freshly returned by the kernel and owned solely by us.
        unsafe { OwnedFd::from_raw_fd(fd as RawFd) }
    }

    /// Regression: the carve opens every entry of a carved directory, and an
    /// ordinary `open` of a FIFO with no writer blocks forever — so a single
    /// named pipe in a worktree root wedged the whole sandbox setup with no
    /// error, no exit and no timeout (it isn't even fail-closed; nothing ever
    /// happens). `O_PATH` never waits for a partner, and the rule is added from
    /// that same descriptor.
    #[test]
    fn writable_root_enumeration_does_not_block_on_a_fifo() {
        let root = tempfile::tempdir().expect("tempdir");
        let git = root.path().join(".git");
        std::fs::create_dir(&git).expect("create .git");
        std::fs::write(root.path().join("file.txt"), "x").expect("write file.txt");
        std::fs::create_dir(root.path().join("sub")).expect("create sub");
        nix::unistd::mkfifo(
            &root.path().join("pipe"),
            nix::sys::stat::Mode::from_bits_truncate(0o600),
        )
        .expect("mkfifo");

        let protected = classify_protected_paths(&[git]).expect("classifying .git");
        let root_path = root.path().to_path_buf();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = pin_path(&root_path).and_then(|root_pin| {
                let plan = plan_filesystem(
                    std::slice::from_ref(&root_pin),
                    &protected,
                    false,
                    false,
                    &|_| true,
                )?;
                let abi = landlock_abi();
                if abi >= 1 {
                    let ruleset = create_test_ruleset(abi);
                    for grant in &plan.grants {
                        add_pin_rule(
                            ruleset.as_raw_fd(),
                            grant,
                            landlock_handled_write_access(abi),
                            abi,
                        )?;
                    }
                }
                Ok(plan.grants.len())
            });
            tx.send(outcome.map_err(|error| format!("{error:#}")))
                .expect("the test thread is waiting");
        });
        let granted = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("planning a writable root containing a FIFO must not block")
            .expect("a FIFO in the root must not fail the plan either");
        assert_eq!(
            granted, 3,
            "file.txt, sub and the FIFO are granted around .git"
        );
    }

    /// A protected path that exists but cannot be pinned must abort the
    /// command, never be dropped: under Landlock a dropped protected entry makes
    /// the worktree root look free of protected descendants, and it is then
    /// granted write *recursively* — which is how `.git/PWNED` became
    /// creatable. One that is not there at all is kept, as missing.
    #[test]
    fn an_unpinnable_protected_path_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");

        // Absent: kept, so that nothing can create it.
        let absent = root.path().join("nowhere/.git");
        let entries =
            classify_protected_paths(&[absent]).expect("an absent protected path is kept");
        assert!(matches!(
            entries.as_slice(),
            [ProtectedEntry {
                kind: ProtectedKind::Missing,
                ..
            }]
        ));

        // A dangling symlink is *not* absent: the entry exists, and a write
        // through it would create its target (a `~/.bashrc -> dotfiles/bashrc`
        // inside a writable home directory then runs at the next login). We
        // cannot pin what it points at, so the command must not run.
        let dangling = root.path().join("dangling");
        std::os::unix::fs::symlink(root.path().join("nowhere"), &dangling).expect("symlink");
        assert!(
            classify_protected_paths(&[dangling]).is_err(),
            "a dangling protected symlink must fail closed"
        );

        // Present but unreachable (its parent cannot be searched): we cannot
        // tell what is there, so the command must not run. Root can search any
        // directory, so as root there is nothing unreachable to try.
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let closed = root.path().join("closed");
        std::fs::create_dir(&closed).expect("create closed");
        std::fs::create_dir(closed.join(".git")).expect("create .git");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 0000");
        let result = classify_protected_paths(&[closed.join(".git")]);
        // Restore the mode first so the tempdir can always clean itself up.
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700))
            .expect("restore mode");
        assert!(
            result.is_err(),
            "an unpinnable protected path must fail closed"
        );
    }

    /// `O_PATH` needs no read permission, so the common case behind the
    /// fail-open above — a `.git` at mode 0300 — now pins normally and is
    /// protected the usual way rather than aborting the command.
    #[test]
    fn an_unreadable_protected_directory_still_pins() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");
        let git = root.path().join(".git");
        std::fs::create_dir(&git).expect("create .git");
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o300)).expect("chmod 0300");

        let entries = classify_protected_paths(std::slice::from_ref(&git));
        // Restore the mode before asserting, so the temp dir can clean itself up
        // (its recursive delete has to read this directory) either way.
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).expect("restore");
        assert!(
            matches!(
                entries.expect("a 0300 directory must still pin").as_slice(),
                [ProtectedEntry {
                    kind: ProtectedKind::Present(_),
                    ..
                }]
            ),
            "the protected path must be pinned, not skipped"
        );
    }

    /// The built-in set is resolved against `$HOME`, including the multi-segment
    /// and file entries, and a trailing separator on `$HOME` changes nothing.
    #[test]
    fn builtin_protected_paths_resolve_against_home() {
        let paths = remote_builtin_protected_paths(Path::new("/home/u"));
        for expected in [
            "/home/u/.ssh",
            "/home/u/.zed_server",
            "/home/u/.config/gh",
            "/home/u/.config/fish",
            "/home/u/.docker/config.json",
            "/home/u/.local/share/zed-jp-enhance",
            "/home/u/.local/bin",
            "/home/u/.cargo/bin",
            "/home/u/.bashrc",
            "/home/u/.profile",
        ] {
            assert!(
                paths.contains(&PathBuf::from(expected)),
                "{expected} must always be protected"
            );
        }
        assert!(
            paths.iter().all(|path| path.starts_with("/home/u")),
            "every built-in entry is home-relative"
        );
        assert_eq!(
            remote_builtin_protected_paths(Path::new("/home/u/")),
            paths,
            "a trailing separator on $HOME must not change the resolved paths"
        );
    }

    /// Built-in entries that don't exist are kept as missing rather than
    /// failing the command, and a **file** entry pins as a non-directory (so the
    /// rule layer narrows its Landlock rights instead of tripping `EINVAL`).
    #[test]
    fn builtin_protected_paths_pin_the_entries_that_exist() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".ssh")).expect("create .ssh");
        std::fs::write(home.path().join(".bashrc"), "x").expect("write .bashrc");

        let entries = classify_protected_paths(&remote_builtin_protected_paths(home.path()))
            .expect("absent built-in entries must not be fatal");
        assert_eq!(entries.len(), UNIX_PROTECTED_HOME_RELATIVE.len());
        let pins: Vec<&CanonicalPathBuf> = entries.iter().filter_map(ProtectedEntry::pin).collect();
        assert_eq!(pins.len(), 2, "only the two entries that exist are pinned");

        let directory = pins
            .iter()
            .find(|pin| pin.path().ends_with(".ssh"))
            .expect(".ssh must be pinned");
        let file = pins
            .iter()
            .find(|pin| pin.path().ends_with(".bashrc"))
            .expect(".bashrc must be pinned");
        assert!(fd_is_dir(directory.fd().as_raw_fd()).expect("fstat .ssh"));
        assert!(
            !fd_is_dir(file.fd().as_raw_fd()).expect("fstat .bashrc"),
            "a file entry must pin as a non-directory"
        );
    }

    /// The built-ins are added to the client's list, not substituted for it, and
    /// a path the client already named is not pinned twice.
    #[test]
    fn merge_adds_the_builtins_without_duplicating() {
        let merged = merge_remote_builtin_protected_paths(
            vec![
                PathBuf::from("/srv/proj/.git"),
                PathBuf::from("/home/u/.ssh"),
            ],
            Path::new("/home/u"),
        );
        assert_eq!(
            merged.first(),
            Some(&PathBuf::from("/srv/proj/.git")),
            "the client's `.git` list is kept"
        );
        assert_eq!(
            merged
                .iter()
                .filter(|path| path.as_path() == Path::new("/home/u/.ssh"))
                .count(),
            1,
            "a path the client already sent must not be added twice"
        );
        assert_eq!(merged.len(), 1 + UNIX_PROTECTED_HOME_RELATIVE.len());
    }

    /// Landlock end-to-end for the shape the built-in set creates: with the
    /// worktree *being* the home directory, a protected **file** (`.bashrc`), a
    /// protected **nested** directory (`.config/gh`) and a **missing** entry
    /// (`.bash_login`) must stay out of reach — the nested one not even movable
    /// out of the way by renaming its parent, the missing one not creatable —
    /// while their siblings stay writable, and a symlink pointing out of the
    /// worktree must not smuggle a write grant to its target.
    ///
    /// `landlock_restrict_self` and `PR_SET_NO_NEW_PRIVS` restrict the calling
    /// *thread*, so enforcing in a dedicated one leaves the rest of the test
    /// process unrestricted.
    #[test]
    fn landlock_protects_files_and_nested_dirs_inside_a_writable_root() {
        let abi = landlock_abi();
        if abi < 1 {
            // No Landlock on this kernel (18.04's 5.4), which uses the mount
            // jail instead.
            return;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let private_tmp = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(".bashrc"), "original\n").expect("write .bashrc");
        std::fs::write(root.path().join("other.txt"), "x").expect("write other.txt");
        std::fs::create_dir(root.path().join("sub")).expect("create sub");
        std::fs::create_dir_all(root.path().join(".config/gh")).expect("create .config/gh");
        std::fs::create_dir(root.path().join(".config/nvim")).expect("create .config/nvim");
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape"))
            .expect("symlink escape");

        let protected = classify_protected_paths(&[
            root.path().join(".bashrc"),
            root.path().join(".config/gh"),
            root.path().join(".bash_login"),
        ])
        .expect("classifying the protected entries");
        let writable = pin_writable_paths(&[root.path().to_path_buf()], false)
            .expect("pinning the writable root");
        let plan = plan_filesystem(&writable, &protected, false, false, &|_| true)
            .expect("planning the grants");
        let root_path = root.path().to_path_buf();
        let tmp_path = private_tmp.path().to_path_buf();

        let results = std::thread::spawn(move || -> Result<Vec<(&'static str, bool)>> {
            apply_landlock(abi, &plan.grants, std::slice::from_ref(&tmp_path), false)?;
            let append =
                |path: PathBuf| std::fs::OpenOptions::new().append(true).open(path).is_ok();
            let create = |path: PathBuf| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
                    .is_ok()
            };
            Ok(vec![
                (".bashrc", append(root_path.join(".bashrc"))),
                ("other.txt", append(root_path.join("other.txt"))),
                ("sub/new.txt", create(root_path.join("sub/new.txt"))),
                (
                    ".config/gh/hijack",
                    create(root_path.join(".config/gh/hijack")),
                ),
                (
                    ".config/nvim/init.lua",
                    create(root_path.join(".config/nvim/init.lua")),
                ),
                ("new-in-root.txt", create(root_path.join("new-in-root.txt"))),
                (".bash_login", create(root_path.join(".bash_login"))),
                (
                    "rename .config",
                    std::fs::rename(root_path.join(".config"), root_path.join(".config-moved"))
                        .is_ok(),
                ),
                ("escape/new.txt", create(root_path.join("escape/new.txt"))),
                ("tmp/new.txt", create(tmp_path.join("new.txt"))),
            ])
        })
        .join()
        .expect("the enforcing thread panicked")
        .expect("applying landlock");
        let allowed: std::collections::HashMap<&str, bool> = results.into_iter().collect();

        assert!(
            !allowed[".bashrc"],
            "a protected file inside the writable root must not be writable"
        );
        assert!(
            !allowed[".config/gh/hijack"],
            "a protected directory nested below the writable root must not be writable"
        );
        assert!(
            !allowed["escape/new.txt"],
            "a symlink out of the worktree must not carry a write grant to its target"
        );
        assert!(
            allowed["other.txt"],
            "an ordinary file beside a protected one stays writable"
        );
        assert!(
            allowed["sub/new.txt"],
            "creating files in an existing subdirectory stays allowed"
        );
        assert!(
            allowed[".config/nvim/init.lua"],
            "a sibling of the protected nested directory stays writable"
        );
        assert!(
            !allowed["new-in-root.txt"],
            "the accepted trade-off: a carved root cannot take brand-new entries \
             (Landlock cannot allow that without also allowing them in `.ssh`)"
        );
        assert!(
            !allowed[".bash_login"],
            "a missing protected entry must not be creatable"
        );
        assert!(
            !allowed["rename .config"],
            "the parent of a protected entry must not be renamable out of the way"
        );
        assert!(
            allowed["tmp/new.txt"],
            "the private temp dir stays writable"
        );
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let path = std::fs::canonicalize(root.path()).expect("canonical tempdir");
        (root, path)
    }

    fn granted(plan: &FilesystemPlan) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = plan
            .grants
            .iter()
            .map(|pin| pin.path().to_path_buf())
            .collect();
        paths.sort();
        paths
    }

    /// A `.git` directly in a worktree root is bound where names are exact,
    /// leaving the root whole. Where they are not (DrvFs), and where no binds
    /// are made (an SSH host), the root is carved instead.
    #[test]
    fn a_direct_child_is_bound_only_where_names_are_exact() {
        let (_root, root) = canonical_tempdir();
        std::fs::create_dir(root.join(".git")).expect("create .git");
        std::fs::write(root.join("file.txt"), "x").expect("write file.txt");
        std::fs::create_dir(root.join("src")).expect("create src");
        let protected = classify_protected_paths(&[root.join(".git")]).expect("classify");
        let roots = pin_writable_paths(std::slice::from_ref(&root), true).expect("pin the root");

        let exact = plan_filesystem(&roots, &protected, false, true, &|_| true).expect("plan");
        assert_eq!(granted(&exact), std::slice::from_ref(&root));
        assert_eq!(exact.binds.len(), 1);

        let carved = [root.join("file.txt"), root.join("src")];
        let inexact = plan_filesystem(&roots, &protected, false, true, &|_| false).expect("plan");
        assert_eq!(granted(&inexact), carved);
        assert!(inexact.binds.is_empty());

        let unbound = plan_filesystem(&roots, &protected, false, false, &|_| true).expect("plan");
        assert_eq!(granted(&unbound), carved);
    }

    /// A protected entry deeper in a root carves every directory on the way
    /// down, binds or not: renaming any of them away would let the command
    /// recreate the path with contents of its own.
    #[test]
    fn a_deep_protected_entry_carves_its_whole_chain() {
        let (_root, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join(".cache/zed-jp-enhance/sandbox/key"))
            .expect("create the install directory");
        std::fs::create_dir(root.join(".cache/other")).expect("create .cache/other");
        std::fs::create_dir(root.join(".cache/zed-jp-enhance/other")).expect("create a sibling");
        std::fs::write(root.join("notes.txt"), "x").expect("write notes.txt");
        let protected = classify_protected_paths(&[root.join(".cache/zed-jp-enhance/sandbox")])
            .expect("classify");
        let roots = pin_writable_paths(std::slice::from_ref(&root), true).expect("pin the root");

        let plan = plan_filesystem(&roots, &protected, false, true, &|_| true).expect("plan");
        assert_eq!(
            granted(&plan),
            [
                root.join(".cache/other"),
                root.join(".cache/zed-jp-enhance/other"),
                root.join("notes.txt"),
            ]
        );
        assert!(plan.binds.is_empty());
    }

    /// A missing entry keeps the directory that would hold it from taking new
    /// entries — and where that directory is missing too, its own parent.
    #[test]
    fn a_missing_protected_entry_carves_where_it_would_be_created() {
        let (_root, root) = canonical_tempdir();
        std::fs::write(root.join("existing.txt"), "x").expect("write existing.txt");
        std::fs::create_dir_all(root.join(".config/nvim")).expect("create .config/nvim");
        let protected = classify_protected_paths(&[
            root.join(".bash_login"),
            root.join(".config/fish/config.fish"),
        ])
        .expect("classify");
        let roots = pin_writable_paths(std::slice::from_ref(&root), true).expect("pin the root");

        let plan = plan_filesystem(&roots, &protected, false, true, &|_| true).expect("plan");
        assert_eq!(
            granted(&plan),
            [root.join(".config/nvim"), root.join("existing.txt")]
        );
    }

    /// A protected symlink keeps its own directory carved and its target out
    /// of every grant.
    #[test]
    fn a_protected_symlink_protects_the_link_and_its_target() {
        let (_root, root) = canonical_tempdir();
        std::fs::create_dir(root.join("dotfiles")).expect("create dotfiles");
        std::fs::write(root.join("dotfiles/bashrc"), "x").expect("write bashrc");
        std::fs::write(root.join("dotfiles/vimrc"), "x").expect("write vimrc");
        std::os::unix::fs::symlink(root.join("dotfiles/bashrc"), root.join(".bashrc"))
            .expect("symlink");
        std::fs::write(root.join("notes.txt"), "x").expect("write notes.txt");
        let protected = classify_protected_paths(&[root.join(".bashrc")]).expect("classify");
        let roots = pin_writable_paths(std::slice::from_ref(&root), true).expect("pin the root");

        let plan = plan_filesystem(&roots, &protected, false, true, &|_| true).expect("plan");
        assert_eq!(
            granted(&plan),
            [root.join("dotfiles/vimrc"), root.join("notes.txt")]
        );
        assert!(plan.binds.is_empty(), "a bind would follow the link");
    }

    /// The WSL home opened as a project: its protected
    /// `.zed-jp-enhance-claude` holds the session's own configuration, which
    /// a read-only bind of it would take along. It is carved instead, and
    /// the configuration keeps its own grant.
    #[test]
    fn a_protected_entry_holding_a_writable_root_is_carved_not_bound() {
        let (_root, root) = canonical_tempdir();
        let configuration = root.join(".zed-jp-enhance-claude/projects/0123456789abcdef");
        std::fs::create_dir_all(configuration.join("config")).expect("create the configuration");
        std::fs::create_dir(root.join(".zed-jp-enhance-claude/projects/other"))
            .expect("create another project's");
        std::fs::write(root.join("notes.txt"), "x").expect("write notes.txt");
        let protected =
            classify_protected_paths(&[root.join(".zed-jp-enhance-claude")]).expect("classify");

        let home_only = pin_writable_paths(std::slice::from_ref(&root), true).expect("pin");
        let plan = plan_filesystem(&home_only, &protected, false, true, &|_| true).expect("plan");
        assert_eq!(plan.binds.len(), 1, "on its own it is bound read-only");

        let roots = pin_writable_paths(&[root.clone(), configuration.clone()], true).expect("pin");
        let plan = plan_filesystem(&roots, &protected, false, true, &|_| true).expect("plan");
        assert!(plan.binds.is_empty());
        assert_eq!(granted(&plan), [configuration, root.join("notes.txt")]);
    }

    /// An invocation that writes `roots` and nothing else, as the client's own
    /// argv decodes into one.
    fn invocation_writing(roots: &[PathBuf]) -> Stage1Invocation {
        let written: Vec<String> = roots
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        let mut argv = build_remote_sandbox_helper_argv(
            &written,
            &[],
            &[],
            &RemoteSandboxNetwork::Blocked,
            false,
            None,
        );
        argv.push("/bin/true".to_string());
        decode_for_test(&argv[1..])
    }

    /// ADR-F49 decisions 14 and 15: the agents' configuration directory and
    /// this fork's data directory are covered only around what the command was
    /// granted inside them, and only where they really are.
    #[test]
    fn an_enclosure_is_covered_only_around_what_is_granted_in_it() {
        let (_home, home) = canonical_tempdir();
        let configurations = home.join(SANDBOXED_AGENT_CONFIG_HOME_RELATIVE);
        let projects = home.join(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE);
        let own = projects.join("0123456789abcdef").join("config");
        let data = home.join(".local/share/zed-jp-enhance");
        let agent = data.join("external_agents/claude");
        std::fs::create_dir_all(&own).expect("create the configuration");
        std::fs::create_dir(projects.join("fedcba9876543210")).expect("create another");
        std::fs::create_dir_all(&agent).expect("create an agent");
        let work = home.join("work");
        std::fs::create_dir(&work).expect("create a worktree");
        let pins = |paths: &[PathBuf]| pin_writable_paths(paths, true).expect("pin");
        let enclosures = enclosed_directories(&home, &data);
        assert_eq!(enclosures, vec![configurations.clone(), data.clone()]);

        let session = pins(&[work.clone(), own.clone()]);
        assert_eq!(
            enclosures_to_cover(&enclosures, &session, &[]),
            std::slice::from_ref(&configurations),
            "the configuration directory is covered around the session's own"
        );
        let readable = pin_readable_paths(std::slice::from_ref(&agent)).expect("pin what is read");
        assert_eq!(
            enclosures_to_cover(&enclosures, &session, &readable),
            vec![configurations.clone(), data],
            "and the data directory around what may be read in it"
        );
        let home_session = pins(&[home.clone(), own]);
        assert_eq!(
            enclosures_to_cover(&enclosures, &home_session, &[]),
            std::slice::from_ref(&configurations),
            "a root above it does not stop the cover"
        );
        assert!(
            enclosures_to_cover(&enclosures, &pins(std::slice::from_ref(&work)), &[]).is_empty()
        );
        assert!(
            enclosures_to_cover(
                &enclosures,
                &pins(std::slice::from_ref(&configurations)),
                &[]
            )
            .is_empty()
        );

        let moved = home.join("moved");
        std::fs::rename(&configurations, &moved).expect("move it away");
        std::os::unix::fs::symlink(&moved, &configurations).expect("symlink");
        let through_link = pins(&[moved.join("projects/0123456789abcdef/config")]);
        assert!(
            enclosures_to_cover(&enclosures, &through_link, &[]).is_empty(),
            "a symlink in its place is not covered"
        );
    }

    /// ADR-F49 decision 14: what may be read is checked before anything runs
    /// and refused naming the flag; once accepted, the data directory holding
    /// it is covered around it, so the thread database and the logs are not
    /// there at all, while the credentials are hidden as ever.
    #[test]
    fn readable_paths_are_checked_then_covered_around() {
        let (_home, home) = canonical_tempdir();
        let data = home.join(".local/share/zed-jp-enhance");
        let agent = data.join("external_agents/registry/npx/claude");
        std::fs::create_dir_all(&agent).expect("create an agent");
        std::fs::create_dir_all(data.join("db")).expect("create the database");
        std::fs::create_dir_all(home.join(".ssh")).expect("create .ssh");

        let refusal = resolve_readable_paths(&[agent.clone(), home.join(".ssh")], &data, &home)
            .expect_err("a credential store may not be shown");
        assert!(
            format!("{refusal:#}").contains(&format!("--readable {}", home.join(".ssh").display())),
            "{refusal:#}"
        );

        let readable = resolve_readable_paths(std::slice::from_ref(&agent), &data, &home)
            .expect("an agent installation may be shown");
        let readable_pins = pin_readable_paths(&readable).expect("pin what is read");
        let invocation = invocation_writing(&[]);
        let reachable = reachable_paths(&invocation, &home);
        let (enclosures, targets) = read_hiding_plan(
            &read_hidden_home_paths(&home, false),
            &reachable,
            &enclosed_directories(&home, &data),
            &[],
            &readable_pins,
        );
        assert_eq!(
            enclosures,
            vec![data.clone()],
            "the data directory is covered around what may be read"
        );
        assert!(targets.contains(&home.join(".ssh")), "{targets:?}");
        assert!(
            !targets.iter().any(|target| target.starts_with(&data)),
            "what is in the data directory is hidden by its cover: {targets:?}"
        );
        check_nothing_shown_is_covered(&[agent], &targets)
            .expect("what may be read is not covered");
    }

    /// ADR-F49 decisions 14 and 27 (final review, F-1): when the remote host
    /// moved its data directory with `XDG_DATA_HOME`, the helper — which has no
    /// registration to consult, running as a separate process from the server —
    /// follows it there. The agent installations and Node.js may be read out of
    /// the moved directory, and its cover binds them back where they really
    /// are, exactly as the read hiding hides the rest of it. With the variable
    /// unset the home-relative default is unchanged.
    #[test]
    fn a_moved_data_directory_is_where_readable_and_its_cover_land() {
        use std::sync::Mutex;
        // Env reads and writes are process-wide; serialise the tests that touch
        // them so setting `XDG_DATA_HOME` here cannot race one reading it.
        static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());
        let _environment = ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        let (_home, home) = canonical_tempdir();
        let (_moved, moved) = canonical_tempdir();
        let previous = std::env::var_os("XDG_DATA_HOME");

        // The default first, with the variable explicitly unset, so the check
        // holds however the test environment was configured.
        // SAFETY: the environment lock serialises this against the other env
        // tests, and `previous` is restored before the guard is dropped.
        unsafe { std::env::remove_var("XDG_DATA_HOME") };
        assert_eq!(
            remote_agent_runtime_data_directory(&home),
            home.join(".local/share/zed-jp-enhance"),
            "with XDG_DATA_HOME unset the home-relative default is unchanged"
        );

        // SAFETY: as above.
        unsafe { std::env::set_var("XDG_DATA_HOME", &moved) };
        let data = remote_agent_runtime_data_directory(&home);
        assert_eq!(
            data,
            moved.join("zed-jp-enhance"),
            "the helper follows XDG_DATA_HOME, not the home-relative default"
        );

        let agent = data.join("external_agents/registry/npx/claude");
        let node = data.join("node/bin");
        std::fs::create_dir_all(&agent).expect("create an agent");
        std::fs::create_dir_all(&node).expect("create the node runtime");
        std::fs::create_dir_all(data.join("db")).expect("create the database");
        std::fs::create_dir_all(home.join(".ssh")).expect("create .ssh");

        let readable = resolve_readable_paths(&[agent.clone(), node.clone()], &data, &home)
            .expect("the agent and node in the moved data directory may be shown");
        let readable_pins = pin_readable_paths(&readable).expect("pin what is read");

        // The read hiding is fed the same env-named directory in production
        // (`app_state_directories_from_environment`, helper `run_stage1`), so
        // the rest of the moved data directory is hidden there.
        let mut read_hidden = read_hidden_home_paths(&home, false);
        read_hidden.extend(app_state_directories_from_environment());
        let invocation = invocation_writing(&[]);
        let reachable = reachable_paths(&invocation, &home);
        let (enclosures, targets) = read_hiding_plan(
            &read_hidden,
            &reachable,
            &enclosed_directories(&home, &data),
            &[],
            &readable_pins,
        );
        assert!(
            enclosures.contains(&data),
            "the moved data directory is covered around what may be read: {enclosures:?}"
        );
        assert!(
            !targets.iter().any(|target| target.starts_with(&data)),
            "what is in the moved data directory is hidden by its cover: {targets:?}"
        );
        check_nothing_shown_is_covered(&[agent, node], &targets)
            .expect("what may be read is not covered");

        // SAFETY: as above; restore what was there before the guard is dropped.
        unsafe {
            match previous {
                Some(value) => std::env::set_var("XDG_DATA_HOME", value),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }

    /// ADR-F49 decision 11: a link planted where the Windows side stages agent
    /// binaries cannot take the cover off what it leads to — the credential
    /// store stays hidden and the link is simply not kept — and what is shown
    /// is never allowed to overlap a cover.
    #[test]
    fn a_link_in_the_staging_directory_does_not_un_hide_a_credential_store() {
        let (_home, home) = canonical_tempdir();
        let keys = home.join(".ssh");
        std::fs::create_dir_all(&keys).expect("create .ssh");
        let staging = home.join(WSL_SANDBOX_HELPER_HOME_RELATIVE);
        std::fs::create_dir_all(&staging).expect("create the staging directory");
        std::os::unix::fs::symlink(&keys, staging.join("agents")).expect("symlink");

        let mut invocation = invocation_writing(&[]);
        invocation.on_wsl = true;
        let reachable = reachable_paths(&invocation, &home);
        assert!(
            reachable.is_empty(),
            "a link where the staged binaries go is not kept: {reachable:?}"
        );
        let targets = read_hidden_targets(&read_hidden_home_paths(&home, false), &reachable, &[]);
        assert!(
            targets.contains(&keys),
            "the credential store stays hidden: {targets:?}"
        );

        // And were something inside a covered entry shown anyway — a
        // `--readable` that resolved there, or a staged directory that did —
        // the command would be refused rather than run with the cover left
        // off.
        let refusal = check_nothing_shown_is_covered(&[keys.join("id_ed25519")], &targets)
            .expect_err("what is shown may not overlap what is hidden");
        assert!(
            format!("{refusal:#}").contains(&keys.display().to_string()),
            "{refusal:#}"
        );
    }

    /// ADR-F49 decision 15: a writable root that is exactly one agent's own
    /// configuration directory for one project, named where it lies, is granted
    /// whole — the agent can create its credentials right at its top — while a
    /// worktree is carved around the agent configurations a host agent acts on.
    #[test]
    fn an_agent_configuration_root_is_granted_whole_and_a_worktree_is_carved() {
        let (_home, home) = canonical_tempdir();
        let configuration = home
            .join(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE)
            .join("0123456789abcdef/config");
        let work = home.join("work");
        std::fs::create_dir_all(&configuration).expect("create the configuration");
        std::fs::create_dir(&work).expect("create a worktree");
        std::fs::write(work.join("main.rs"), "x").expect("write main.rs");
        let roots = [configuration.clone(), work.clone()];

        let invocation = invocation_writing(&roots);
        let pins = pin_writable_paths(&roots, true).expect("pin the roots");
        let protected =
            classify_protected_paths(&worktree_protected_additions(&invocation, &pins, &home))
                .expect("classify");
        let plan = plan_filesystem(&pins, &protected, false, false, &|_| true).expect("plan");
        let granted = granted(&plan);
        assert!(granted.contains(&configuration), "{granted:?}");
        assert!(!granted.contains(&work), "{granted:?}");
        assert!(granted.contains(&work.join("main.rs")), "{granted:?}");
    }

    /// ADR-F49 decision 15: only a root at exactly that depth, named where it
    /// lies, is one agent's own configuration. A project's directory itself, a
    /// directory deeper inside one, and a link in the projects directory
    /// pointing at a worktree are worktrees like any other — a link there would
    /// otherwise strip the worktree's own protections.
    #[test]
    fn only_a_root_where_an_agent_configuration_sits_skips_the_worktree_entries() {
        let (_home, home) = canonical_tempdir();
        let projects = home.join(SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE);
        let project = projects.join("0123456789abcdef");
        let configuration = project.join("config");
        let deeper = configuration.join("plugins");
        let work = home.join("work");
        std::fs::create_dir_all(&deeper).expect("create the configuration");
        std::fs::create_dir(&work).expect("create a worktree");
        let link = projects.join("link");
        std::os::unix::fs::symlink(&work, &link).expect("symlink");

        let carved = |root: &Path| {
            let roots = [root.to_path_buf()];
            let invocation = invocation_writing(&roots);
            let pins = pin_writable_paths(&roots, false).expect("pin the root");
            !worktree_protected_additions(&invocation, &pins, &home).is_empty()
        };
        assert!(!carved(&configuration), "one agent's own configuration");
        assert!(carved(&project), "a project's directory holds several");
        assert!(carved(&deeper), "deeper inside one is not one");
        assert!(carved(&work), "a worktree");
        assert!(
            carved(&link),
            "a link in the projects directory leading to a worktree is a worktree"
        );
    }

    #[test]
    fn join_absolute_places_path_under_base() {
        assert_eq!(
            join_absolute(Path::new("/tmp/root"), Path::new("/home/u/p")),
            PathBuf::from("/tmp/root/home/u/p")
        );
    }

    #[test]
    fn non_directory_entry_access_omits_directory_only_rights() {
        // The bug this guards: granting a directory-only access on a regular
        // file's fd makes `landlock_add_rule` fail with EINVAL, which failed the
        // whole sandbox closed and blocked every command on 24.04.
        let abi = 3;
        let handled = landlock_handled_write_access(abi);
        let dir = landlock_entry_write_access(true, abi, handled);
        let file = landlock_entry_write_access(false, abi, handled);

        let directory_only = LANDLOCK_ACCESS_FS_MAKE_DIR
            | LANDLOCK_ACCESS_FS_MAKE_REG
            | LANDLOCK_ACCESS_FS_MAKE_CHAR
            | LANDLOCK_ACCESS_FS_MAKE_SOCK
            | LANDLOCK_ACCESS_FS_MAKE_FIFO
            | LANDLOCK_ACCESS_FS_MAKE_BLOCK
            | LANDLOCK_ACCESS_FS_MAKE_SYM
            | LANDLOCK_ACCESS_FS_REMOVE_DIR
            | LANDLOCK_ACCESS_FS_REMOVE_FILE
            | LANDLOCK_ACCESS_FS_REFER;

        assert_eq!(
            file & directory_only,
            0,
            "a non-directory entry must carry no directory-only access"
        );
        assert!(
            file & LANDLOCK_ACCESS_FS_WRITE_FILE != 0,
            "a non-directory entry must still be writable"
        );
        assert_eq!(dir & directory_only, directory_only & handled);
        // Every granted bit must be one the ruleset handles, or the kernel
        // rejects the rule with EINVAL regardless of inode type.
        assert_eq!(file & !handled, 0);
        assert_eq!(dir & !handled, 0);
    }

    /// The Windows side's IDE flags decode into the forward the helper serves,
    /// and every malformed combination is refused.
    #[test]
    fn ide_forward_flags_round_trip_and_are_checked() {
        let session = crate::InteractiveSession {
            ide_forward: Some(crate::IdeForward {
                inner_port: 45123,
                rendezvous_path: "/root/.cache/zed-jp-enhance/ide/s/bridge.sock".to_string(),
            }),
        };
        let mut argv = crate::build_wsl_sandbox_helper_argv(
            &[],
            &[],
            &[],
            &RemoteSandboxNetwork::Blocked,
            false,
            None,
            &crate::WslHelperOptions {
                interactive_session: Some(&session),
                ..crate::WslHelperOptions::default()
            },
        );
        argv.push("/bin/true".to_string());
        let decoded = decode_for_test(&argv[1..]);
        assert_eq!(
            decoded.ide_forward,
            Some(IdeForwardRequest {
                port: 45123,
                rendezvous: PathBuf::from("/root/.cache/zed-jp-enhance/ide/s/bridge.sock"),
            })
        );

        let parse = |args: &[&str]| parse_stage1_args(args.iter().map(OsString::from));
        assert!(
            parse(&["--interactive", "--", "true"])
                .expect("an interactive session needs no IDE forward")
                .ide_forward
                .is_none()
        );
        for invalid in [
            &[
                "--ide-listen",
                "1",
                "--ide-rendezvous",
                "/r/s",
                "--",
                "true",
            ][..],
            &[
                "--interactive",
                "--ide-listen",
                "0",
                "--ide-rendezvous",
                "/r/s",
                "--",
                "true",
            ],
            &[
                "--interactive",
                "--ide-listen",
                "65536",
                "--ide-rendezvous",
                "/r/s",
                "--",
                "true",
            ],
            &["--interactive", "--ide-listen", "1", "--", "true"],
            &["--interactive", "--ide-rendezvous", "/r/s", "--", "true"],
            &[
                "--interactive",
                "--ide-listen",
                "1",
                "--ide-rendezvous",
                "relative",
                "--",
                "true",
            ],
        ] {
            assert!(parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn the_holder_release_carries_both_ports() {
        for (proxy_port, ide_port) in [(0, 0), (8080, 0), (0, 45123), (65535, 1)] {
            assert_eq!(
                decode_holder_release(encode_holder_release(proxy_port, ide_port)),
                (proxy_port, ide_port)
            );
        }
    }

    #[test]
    fn process_state_and_parent_are_read_after_the_command_name() {
        assert_eq!(
            parse_process_state_and_parent("42 (sleep) S 7 42 42 0 -1"),
            Some(('S', 7))
        );
        assert_eq!(
            parse_process_state_and_parent("43 (a) b (c)) Z 1 0"),
            Some(('Z', 1))
        );
        assert_eq!(parse_process_state_and_parent("44 (x"), None);
        assert_eq!(parse_process_state_and_parent(""), None);
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "the test needs a real child process of its own"
    )]
    fn a_running_child_is_among_the_living_children() {
        let mut child = Command::new("sleep").arg("5").spawn().expect("spawn sleep");
        let pid = libc::pid_t::try_from(child.id()).expect("a pid");
        let found = living_children().contains(&pid);
        child.kill().expect("stop sleep");
        child.wait().expect("reap sleep");
        assert!(found, "a running child of this process is listed");
    }

    #[test]
    fn a_relay_serves_no_more_than_its_cap_at_once() {
        use std::io::Read as _;

        let listener =
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("listen");
        let address = listener.local_addr().expect("address");
        spawn_relay(
            OwnedFd::from(listener),
            "test",
            2,
            |mut inbound: std::net::TcpStream| {
                // Held until the client closes its end.
                let mut byte = [0u8; 1];
                while matches!(inbound.read(&mut byte), Ok(count) if count > 0) {}
            },
        )
        .expect("start the relay");
        let connect = |timeout: Duration| {
            let stream = std::net::TcpStream::connect(address).expect("connect");
            stream
                .set_read_timeout(Some(timeout))
                .expect("set a read timeout");
            stream
        };
        let refused = |mut stream: std::net::TcpStream| {
            let mut byte = [0u8; 1];
            matches!(stream.read(&mut byte), Ok(0))
        };

        let first = connect(Duration::from_secs(5));
        let _second = connect(Duration::from_secs(5));
        assert!(
            refused(connect(Duration::from_secs(5))),
            "one beyond the cap is closed at once"
        );
        drop(first);
        // Its place frees once its thread has seen the close.
        let served_again = (0..100).any(|_| {
            let served = !refused(connect(Duration::from_millis(200)));
            if !served {
                std::thread::sleep(Duration::from_millis(50));
            }
            served
        });
        assert!(served_again, "a closed connection gives its place back");
    }

    /// The IDE rendezvous has to be a private directory of this account's
    /// inside the protected set, and only a real socket there is connected to.
    #[test]
    fn the_ide_rendezvous_is_checked_before_and_at_every_connection() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;

        let (_root, root) = canonical_tempdir();
        let protected_directory = root.join("protected");
        let rendezvous_directory = protected_directory.join("ide");
        std::fs::create_dir_all(&rendezvous_directory).expect("create the rendezvous");
        std::fs::create_dir(root.join("elsewhere")).expect("create elsewhere");
        let private = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&rendezvous_directory, private.clone()).expect("chmod");
        std::fs::set_permissions(root.join("elsewhere"), private.clone()).expect("chmod");
        let protected =
            classify_protected_paths(std::slice::from_ref(&protected_directory)).expect("classify");
        let writable = pin_writable_paths(&[root.join("elsewhere")], false).expect("pin");
        let socket = rendezvous_directory.join("bridge.sock");

        assert!(
            IdeRendezvous::open(&root.join("elsewhere/bridge.sock"), &protected, &writable)
                .is_err(),
            "outside the protected set"
        );
        // A rendezvous inside a writable grant is refused even if it is also
        // listed as protected.
        let also_writable =
            pin_writable_paths(std::slice::from_ref(&protected_directory), false).expect("pin");
        assert!(
            IdeRendezvous::open(&socket, &protected, &also_writable).is_err(),
            "inside a writable grant"
        );
        std::fs::set_permissions(
            &rendezvous_directory,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        assert!(
            IdeRendezvous::open(&socket, &protected, &writable).is_err(),
            "a directory other accounts can look into"
        );
        std::fs::set_permissions(&rendezvous_directory, private).expect("chmod");
        let rendezvous = IdeRendezvous::open(&socket, &protected, &writable)
            .expect("a private directory inside the protected set");

        assert!(rendezvous.connect().is_err(), "nothing listens yet");
        let listener = UnixListener::bind(&socket).expect("listen");
        assert!(rendezvous.connect().is_ok());
        drop(listener);
        std::fs::remove_file(&socket).expect("remove the socket");
        let other = root.join("other.sock");
        let _other_listener = UnixListener::bind(&other).expect("listen elsewhere");
        std::os::unix::fs::symlink(&other, &socket).expect("plant a symlink");
        let error = rendezvous.connect().expect_err("never through a symlink");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
