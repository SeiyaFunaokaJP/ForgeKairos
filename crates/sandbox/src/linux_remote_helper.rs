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

use anyhow::{Context as _, Result, anyhow, bail};
use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::RemoteSandboxNetwork;
use crate::linux_bubblewrap::install_command_seccomp_filter;
use crate::protected_paths::{
    UNIX_PROTECTED_HOME_RELATIVE, WSL_SANDBOX_HELPER_HOME_RELATIVE, home_dir_from_account,
    merge_remote_builtin_protected_paths, zsh_startup_files_from_environment,
};
use crate::util::CanonicalPathBuf;
use crate::{
    REMOTE_SANDBOX_HELPER_FLAG, WSL_HELPER_MODE_FLAG, WSL_PROBE_RESULT_PREFIX,
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

/// Handle a possible re-exec of this binary as the remote sandbox helper, or as
/// one of the two short-lived WSL utilities the Windows backend runs
/// ([`WSL_SANDBOX_RESOLVE_FLAG`], [`WSL_SANDBOX_PROBE_FLAG`]). None of them
/// returns if it matched. Called from
/// [`crate::run_remote_sandbox_helper_if_invoked`] at the top of `main`.
pub(crate) fn run_if_invoked() {
    let args: Vec<OsString> =
        crate::expand_remote_sandbox_helper_argv(std::env::args_os().skip(1).collect());
    let Some((flag, rest)) = args.split_first() else {
        return;
    };
    if flag != WSL_SANDBOX_RESOLVE_FLAG
        && flag != WSL_SANDBOX_PROBE_FLAG
        && flag != REMOTE_SANDBOX_HELPER_FLAG
    {
        return;
    }
    close_verified_exec_descriptor();
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

fn init_helper_logging() {
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
    program: OsString,
    args: Vec<OsString>,
}

fn parse_stage1_args(args: impl IntoIterator<Item = OsString>) -> Result<Stage1Invocation> {
    let mut net_mode: Option<String> = None;
    let mut allowed_hosts: Vec<String> = Vec::new();
    let mut ceiling_entries: Option<Vec<String>> = None;
    let mut allow_fs_write = false;
    let mut writable_paths = Vec::new();
    let mut protected_paths = Vec::new();
    let mut chdir = None;
    let mut wsl = false;
    let mut slirp_sha256 = None;
    let mut set_environment = Vec::new();
    let mut unset_environment = Vec::new();
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

    Ok(Stage1Invocation {
        network,
        network_ceiling,
        allow_fs_write,
        writable_paths,
        protected_paths,
        chdir,
        wsl,
        on_wsl: wsl,
        slirp_sha256,
        set_environment,
        unset_environment,
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
    invocation.protected_paths =
        merge_remote_builtin_protected_paths(invocation.protected_paths, &home);
    for path in zsh_startup_files_from_environment() {
        if !invocation.protected_paths.contains(&path) {
            invocation.protected_paths.push(path);
        }
    }
    if invocation.wsl {
        // Where the Windows side installs this very binary; overwriting it runs
        // code outside the sandbox on the next command (the WSL counterpart of
        // `.zed_server`, which the list above already covers).
        let install_root = home.join(WSL_SANDBOX_HELPER_HOME_RELATIVE);
        if !invocation.protected_paths.contains(&install_root) {
            invocation.protected_paths.push(install_root);
        }
    }
    log::info!(
        "[remote-sandbox] protecting {} paths ({} built-ins under HOME {})",
        invocation.protected_paths.len(),
        UNIX_PROTECTED_HOME_RELATIVE.len(),
        home.display()
    );

    let self_exe = std::env::current_exe().unwrap_or_default();
    let network_on = network_requested(&invocation.network);
    let slirp = if network_on {
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

    // Pinned and planned here, before the fork, so that this process holds
    // every inode the holder relies on until the command exits (see
    // `FilesystemPlan`).
    let original_open_file_limit = raise_open_file_limit();
    let writable_pins = match pin_writable_paths(&invocation.writable_paths, invocation.wsl) {
        Ok(pins) => pins,
        Err(error) => fail_closed("writable-redirected", &format!("{error:#}")),
    };
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
    let ready_write_fd = ready.write.as_raw_fd();
    let go_read_fd = go.read.as_raw_fd();
    let relay_holder_fd = relay.holder.as_raw_fd();

    // SAFETY: a single `fork` from a single-threaded process. The holder becomes
    // its own process; it and the parent coordinate only through the pipes.
    let child_pid = unsafe { libc::fork() };
    if child_pid < 0 {
        fail_closed("fork-failed", "fork() failed");
    }
    if child_pid == 0 {
        // Holder. Never returns. (`invocation` and `fs_backend` are moved in only
        // on this diverging path, so the parent below still owns them.)
        run_holder(
            HolderSetup {
                invocation,
                fs_backend,
                writable_pins: &writable_pins,
                protected: &protected,
                plan: &plan,
                original_open_file_limit,
            },
            ready_write_fd,
            go_read_fd,
            relay_holder_fd,
        );
    }

    // --- Parent -----------------------------------------------------------
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
        match start_egress_proxy(
            &invocation.network,
            invocation.network_ceiling.as_ref(),
            invocation.on_wsl,
        ) {
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

    // Release the holder, handing it the proxy port (0 = no proxy).
    if let Err(error) = write_all_bytes(go_write.as_raw_fd(), &proxy_port.to_le_bytes()) {
        abort_holder(child_pid, "handshake-failed", &error.to_string());
    }

    // Take over the listening socket the holder created inside the network
    // namespace, start splicing it to the proxy, and only then let the command
    // run.
    if network_on {
        match receive_relay_listener(relay_parent.as_raw_fd()) {
            Ok(listener) => {
                if let Err(error) = spawn_egress_relay(listener, proxy_port) {
                    abort_holder(
                        child_pid,
                        "relay-failed",
                        &format!("starting the egress relay: {error}"),
                    );
                }
                // slirp has now done its one remaining job: configuring the
                // namespace (bringing its loopback up) so the listener above
                // could be bound. Stop it before the command starts. Even with
                // `--disable-host-loopback` its NAT can still reach any address
                // the *host itself* owns — a socket bound to `127.0.0.1` is
                // free to connect to the host's own LAN address, which is how
                // `192.168.x.x:22` answered from inside the sandbox. With slirp
                // gone the namespace keeps nothing but its own loopback, so the
                // relayed proxy is the only endpoint that exists.
                if let Some(slirp_process) = slirp_child.as_mut() {
                    stop_slirp(slirp_process);
                }
                slirp_child = None;
                // Release the holder only now, so the command can never observe
                // an outbound path that is about to be taken away.
                if let Err(error) = write_all_bytes(go_write.as_raw_fd(), &[HOLDER_OK]) {
                    abort_holder(child_pid, "handshake-failed", &error.to_string());
                }
            }
            // The holder reaches this point unless it failed closed on its own,
            // in which case it has already printed its reason and will exit
            // 126: surface that rather than inventing a second one.
            Err(error) => {
                log::warn!("[remote-sandbox] the holder sent no egress listener: {error:#}")
            }
        }
    }
    drop(go_write);
    drop(relay_parent);

    let status = wait_pid(child_pid);
    // Only now; see `FilesystemPlan` for why the parent holds these open.
    drop(plan);
    drop(protected);
    drop(writable_pins);

    // Tear down slirp and the proxy now the command has finished.
    if let Some(slirp_process) = slirp_child.as_mut() {
        stop_slirp(slirp_process);
    }
    drop(slirp);
    drop(proxy);
    // The holder created its scratch directories under its own pid and then
    // `exec`ed the command, so we are the one left to remove them — on every
    // path, including the failing ones below.
    remove_command_temp_dirs(child_pid);

    match status {
        Ok(code) => std::process::exit(code),
        Err(_) => fail_closed("wait-failed", "failed to wait for the sandboxed command"),
    }
}

/// Reap the holder, remove anything it left under `/tmp`, and fail closed.
/// Every post-`fork` parent failure goes through here so no path can leak the
/// per-command scratch directories. Never returns.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              blocking on the holder is correct here"
)]
fn abort_holder(child_pid: libc::pid_t, code: &str, detail: &str) -> ! {
    if let Err(error) = wait_pid(child_pid) {
        log::warn!("[remote-sandbox] could not reap the holder: {error:#}");
    }
    remove_command_temp_dirs(child_pid);
    fail_closed(code, detail)
}

const HOLDER_OK: u8 = 1;

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
    protected: &'a [ProtectedEntry],
    plan: &'a FilesystemPlan,
    /// The soft open-file limit to hand back to the command (see
    /// [`raise_open_file_limit`]).
    original_open_file_limit: Option<libc::rlimit>,
}

#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              `CommandExt::exec` of the untrusted command is exactly what it must do"
)]
fn run_holder_inner(
    setup: HolderSetup<'_>,
    ready_write_fd: RawFd,
    go_read_fd: RawFd,
    relay_socket_fd: RawFd,
) -> std::result::Result<std::convert::Infallible, (&'static str, anyhow::Error)> {
    let HolderSetup {
        invocation,
        fs_backend,
        writable_pins,
        protected,
        plan,
        original_open_file_limit,
    } = setup;
    // Create the user + network namespaces (unprivileged NEWNET is only allowed
    // together with NEWUSER). As the creator we hold every capability inside the
    // new user namespace until we `exec`.
    // SAFETY: `unshare` with these flags reads no memory.
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
    // Report readiness, then block until the parent maps our ids and hands us the
    // proxy port. A short read means the parent aborted.
    write_all_bytes(ready_write_fd, &[HOLDER_OK])
        .map_err(|error| ("handshake-failed", anyhow::Error::new(error)))?;
    let mut port_bytes = [0u8; 2];
    read_exact_bytes(go_read_fd, &mut port_bytes).map_err(|error| {
        (
            "handshake-failed",
            anyhow!("parent aborted before release: {error}"),
        )
    })?;
    let proxy_port = u16::from_le_bytes(port_bytes);

    // Publish the proxy *inside* the namespace. slirp has no host-loopback
    // forwarding and no working NAT, so nothing here can reach the host on its
    // own; the parent (which is outside) accepts on this socket and splices to
    // the proxy. Created before the FS jail and the seccomp filter, both of
    // which the command must not be able to work around.
    let relay_port = if proxy_port != 0 {
        let port = publish_relay_listener(relay_socket_fd).map_err(|e| ("relay-failed", e))?;
        // Then wait for the parent to finish taking the namespace's network
        // apart again (see the `slirp_child.kill()` there): until that byte
        // arrives, slirp is still running and its NAT can still reach addresses
        // the host owns.
        let mut released = [0u8; 1];
        read_exact_bytes(go_read_fd, &mut released).map_err(|error| {
            (
                "handshake-failed",
                anyhow!("parent aborted before the network was final: {error}"),
            )
        })?;
        Some(port)
    } else {
        None
    };

    // Apply the filesystem backend to what the parent pinned and planned. Each
    // branch yields the directory the command's `TMPDIR` should point at.
    let tmpdir: PathBuf = match fs_backend {
        FsBackend::Landlock(abi) => {
            let mut temporary_writable = Vec::new();
            let tmpdir = if invocation.wsl {
                enter_private_mount_namespace().map_err(|e| ("mount-failed", e))?;
                let tmpdir = provide_private_tmp(
                    writable_pins,
                    plan,
                    invocation.allow_fs_write,
                    &mut temporary_writable,
                )
                .map_err(|e| ("mount-failed", e))?;
                provide_private_dev_shm(writable_pins, &mut temporary_writable);
                for pin in &plan.binds {
                    bind_read_only(pin).map_err(|e| ("mount-failed", e))?;
                }
                tmpdir
            } else {
                let private_tmp = create_private_tmpdir().map_err(|e| ("mkdtemp-failed", e))?;
                temporary_writable.push(private_tmp.clone());
                private_tmp
            };
            apply_landlock(abi, &plan.grants, &temporary_writable, invocation.on_wsl)
                .map_err(|e| ("landlock-failed", e))?;
            tmpdir
        }
        FsBackend::MountJail => {
            // The jail can only re-bind what exists, so a missing protected
            // entry stays creatable on these kernels (5.4, no Landlock).
            let protected_pins: Vec<CanonicalPathBuf> = protected
                .iter()
                .filter_map(|entry| entry.pin().cloned())
                .collect();
            apply_mount_jail(writable_pins, &protected_pins, invocation.allow_fs_write)
                .map_err(|e| ("mount-failed", e))?;
            PathBuf::from("/tmp")
        }
    };

    // The caller's environment first, so that the sandbox's own temp-dir and
    // proxy settings below always win over it.
    // SAFETY: single-threaded here (a forked, not-yet-exec'd child), so
    // `set_var` / `remove_var` are sound; the names were validated at parse time.
    for (name, value) in &invocation.set_environment {
        unsafe { std::env::set_var(name, value) };
    }
    for name in &invocation.unset_environment {
        unsafe { std::env::remove_var(name) };
    }
    // How a process inside WSL finds the Windows interop socket. The seccomp
    // filter already makes that socket unusable; this stops a command from even
    // trying, whichever transport started the helper.
    if invocation.on_wsl {
        for name in ["WSL_INTEROP", "WSLENV"] {
            // SAFETY: as above.
            unsafe { std::env::remove_var(name) };
        }
    }

    // Point the command's temp-dir env at the writable temp location.
    // SAFETY: single-threaded here (a forked, not-yet-exec'd child), so `set_var`
    // is sound.
    for key in ["TMPDIR", "TMP", "TEMP"] {
        unsafe { std::env::set_var(key, &tmpdir) };
    }
    // Point the command's proxy env at the in-namespace relay listener (the
    // sandbox's own loopback, not the host's).
    if let Some(relay_port) = relay_port {
        let url = format!("http://127.0.0.1:{relay_port}");
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            unsafe { std::env::set_var(key, &url) };
        }
        for key in ["NO_PROXY", "no_proxy"] {
            unsafe { std::env::set_var(key, "") };
        }
    }

    // chdir into the working directory *after* the FS jail is in place (so the
    // path resolves inside the jail).
    if let Some(dir) = &invocation.chdir {
        std::env::set_current_dir(dir)
            .with_context(|| format!("chdir into {}", dir.display()))
            .map_err(|e| ("chdir-failed", e))?;
    }

    restore_open_file_limit(original_open_file_limit);

    // WSL runs commands as root by default, as can a dev container's
    // `remoteUser`, and root in this user namespace would keep every capability
    // across `exec` — `CAP_SYS_ADMIN` over the mount namespace holding the
    // read-only binds or the mount jail included (Bubblewrap drops them the
    // same way). Anyone else loses them at `exec` anyway.
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if invocation.on_wsl || unsafe { libc::geteuid() } == 0 {
        drop_all_capabilities().map_err(|e| ("caps-failed", e))?;
    }

    // Install the AF_UNIX-blocking seccomp filter (shared with the Bubblewrap
    // backend) and exec. The filter survives exec; the command reaches the proxy
    // over TCP (AF_INET), which the filter allows.
    install_command_seccomp_filter().map_err(|e| ("seccomp-failed", e))?;
    let error = Command::new(&invocation.program)
        .args(&invocation.args)
        .exec();
    Err((
        "exec-failed",
        anyhow!("exec {:?}: {error}", invocation.program),
    ))
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

/// Bind the sandbox's egress entry point on the namespace's own loopback and
/// hand the listening socket to the parent, which is the only participant that
/// can still reach the host. Returns the port the command's `HTTP(S)_PROXY`
/// must name.
///
/// Binding here — inside the namespace, before any restriction is applied — is
/// what makes the tighter slirp configuration possible: the parent cannot
/// create a socket in a namespace it is not in, and joining one would mean
/// entering the holder's user namespace and losing the ability to come back
/// out.
fn publish_relay_listener(socket_fd: RawFd) -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .context("binding the in-sandbox egress listener")?;
    let port = listener
        .local_addr()
        .context("reading the in-sandbox egress listener address")?
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

/// Accept on the sandbox-side listener and splice each connection to the egress
/// proxy on the host's loopback. This is the whole of the sandbox's outbound
/// path: it can reach the proxy's allowlist decision and nothing else.
fn spawn_egress_relay(listener: OwnedFd, proxy_port: u16) -> std::io::Result<()> {
    let listener = std::net::TcpListener::from(listener);
    std::thread::Builder::new()
        .name("zed-remote-sandbox-egress".into())
        .spawn(move || {
            loop {
                let inbound = match listener.accept() {
                    Ok((inbound, _peer)) => inbound,
                    Err(error) => {
                        log::warn!("[remote-sandbox] egress relay stopped accepting: {error}");
                        break;
                    }
                };
                if let Err(error) = std::thread::Builder::new()
                    .name("zed-remote-sandbox-egress-conn".into())
                    .spawn(move || relay_connection(inbound, proxy_port))
                {
                    log::warn!("[remote-sandbox] could not relay a connection: {error}");
                }
            }
        })?;
    Ok(())
}

/// Pump one accepted sandbox connection to and from the proxy until both
/// directions end.
fn relay_connection(inbound: std::net::TcpStream, proxy_port: u16) {
    use std::net::{Ipv4Addr, Shutdown, TcpStream};

    let outbound = match TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)) {
        Ok(outbound) => outbound,
        Err(error) => {
            log::warn!("[remote-sandbox] egress relay could not reach the proxy: {error}");
            return;
        }
    };
    let (mut inbound_reader, mut outbound_writer) =
        match (inbound.try_clone(), outbound.try_clone()) {
            (Ok(inbound_reader), Ok(outbound_writer)) => (inbound_reader, outbound_writer),
            _ => {
                log::warn!("[remote-sandbox] egress relay could not duplicate a relayed socket");
                return;
            }
        };

    let to_proxy = std::thread::spawn(move || {
        // A relayed connection normally ends with a reset or a half-close from
        // one side, so a copy error here is expected traffic, not a fault.
        if let Err(error) = std::io::copy(&mut inbound_reader, &mut outbound_writer) {
            log::debug!("[remote-sandbox] egress relay request stream ended: {error}");
        }
        let _ = outbound_writer.shutdown(Shutdown::Write);
    });

    let mut outbound_reader = outbound;
    let mut inbound_writer = inbound;
    if let Err(error) = std::io::copy(&mut outbound_reader, &mut inbound_writer) {
        log::debug!("[remote-sandbox] egress relay response stream ended: {error}");
    }
    if let Err(error) = inbound_writer.shutdown(Shutdown::Write) {
        log::debug!("[remote-sandbox] egress relay could not half-close: {error}");
    }
    if to_proxy.join().is_err() {
        log::warn!("[remote-sandbox] egress relay request thread panicked");
    }
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
    // unless the helper's diagnostics were asked for (see `init_helper_logging`).
    if std::env::var_os("ZED_REMOTE_SANDBOX_LOG").is_none() {
        command
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
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

/// A command's own decision under an any-host grant in WSL mode when no
/// editor-wide allowlist was sent: every host, but only at a public address.
/// From WSL, local addresses include the distro's own services, the Windows
/// host behind the NAT gateway, and with mirrored networking Windows'
/// loopback too. With an allowlist in force it decides about those instead,
/// exactly as the in-process proxy's ceiling does elsewhere.
struct PublicAddressesOnly;

impl http_proxy::DestinationPolicy for PublicAddressesOnly {
    fn evaluate(&self, _host: &str, _port: u16) -> http_proxy::PolicyDecision {
        http_proxy::PolicyDecision::Allow {
            permit_local_addresses: false,
        }
    }
}

/// Stand up the host-loopback egress proxy enforcing the per-command allowlist,
/// nested inside the editor-wide one when the WSL backend sent it.
fn start_egress_proxy(
    network: &RemoteSandboxNetwork,
    network_ceiling: Option<&Arc<crate::EditorAllowlistCeiling>>,
    wsl: bool,
) -> Result<http_proxy::ProxyHandle> {
    use http_proxy::{Allowlist, DestinationPolicy, HostPattern, ProxyConfig, ProxyHandle};

    let policy: Option<Arc<dyn DestinationPolicy>> =
        if wsl && network_ceiling.is_none() && matches!(network, RemoteSandboxNetwork::AnyHost) {
            Some(Arc::new(PublicAddressesOnly))
        } else {
            None
        };
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
/// [`directory_names_are_exact`]).
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
fn create_private_tmpdir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = PathBuf::from(format!("/tmp/zed-remote-sandbox-{}", std::process::id()));
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
        let private_tmp = create_private_tmpdir()?;
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
        let private_tmp = create_private_tmpdir()?;
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

    let new_root = PathBuf::from(format!("/tmp/.zed-remote-jail-{}", std::process::id()));
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

    // Re-bind each writable path read-write over the RO root.
    for pin in writable_pins {
        let target = join_absolute(&new_root, pin.path());
        if !ensure_bind_target(&target) {
            continue;
        }
        mount(
            Some(pin.path()),
            &target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .with_context(|| format!("rw-binding {}", pin.path().display()))?;
        // Verify the bound inode is the one we pinned (TOCTOU backstop).
        verify_same_inode(pin, &target)
            .with_context(|| format!("verifying rw bind of {}", pin.path().display()))?;
    }

    // Re-bind protected paths read-only over the (now writable) worktree.
    for protected_pin in protected_pins {
        let target = join_absolute(&new_root, protected_pin.path());
        if !target.exists() {
            continue;
        }
        mount(
            Some(protected_pin.path()),
            &target,
            None,
            libc::MS_BIND | libc::MS_REC,
            None,
        )
        .with_context(|| format!("ro-binding protected {}", protected_pin.path().display()))?;
        mount(
            None,
            &target,
            None,
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            None,
        )
        .with_context(|| {
            format!(
                "remounting protected {} read-only",
                protected_pin.path().display()
            )
        })?;
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
    use crate::protected_paths::remote_builtin_protected_paths;
    use http_proxy::DestinationPolicy as _;

    /// Round-trip the builder through the stage-1 decoder to lock the argv
    /// protocol both sides share.
    #[test]
    fn stage1_decode_round_trips_the_builder() {
        let mut argv = build_remote_sandbox_helper_argv(
            &["/home/u/proj".to_string(), "/home/u/scratch".to_string()],
            &["/home/u/proj/.git".to_string()],
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
            &RemoteSandboxNetwork::AnyHost,
            false,
            Some("/mnt/c/proj"),
            &crate::WslHelperOptions {
                network_ceiling: Some(&ceiling),
                slirp_sha256: Some("abc123"),
                environment: &environment,
                unset_environment: &["WSL_INTEROP", "WSLENV"],
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

        let proxy = start_egress_proxy(&RemoteSandboxNetwork::AnyHost, Some(&ceiling), true)
            .expect("start the proxy");
        let refused = connect_response(proxy.port(), "evil.example:443");
        assert!(refused.starts_with("HTTP/1.1 511"), "{refused}");
        assert!(refused.contains("Via: 1.1 zed-egress-proxy"), "{refused}");
        let local = connect_response(proxy.port(), &local_target);
        assert!(local.starts_with("HTTP/1.1 200"), "{local}");

        let proxy = start_egress_proxy(
            &RemoteSandboxNetwork::Hosts(vec!["evil.example".to_string()]),
            Some(&ceiling),
            true,
        )
        .expect("start the proxy");
        let refused = connect_response(proxy.port(), "evil.example:443");
        assert!(refused.contains("Via: 1.1 zed-egress-proxy"), "{refused}");
    }

    /// Without an editor-wide allowlist, an any-host grant in WSL mode reaches
    /// public addresses only; an SSH host keeps its documented behavior.
    #[test]
    fn an_uncapped_wsl_any_host_grant_refuses_local_addresses() {
        let local_service =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a local service");
        let local_target = format!(
            "127.0.0.1:{}",
            local_service.local_addr().expect("local address").port()
        );

        let wsl = start_egress_proxy(&RemoteSandboxNetwork::AnyHost, None, true)
            .expect("start the proxy");
        let refused = connect_response(wsl.port(), &local_target);
        assert!(refused.starts_with("HTTP/1.1 511"), "{refused}");

        let ssh = start_egress_proxy(&RemoteSandboxNetwork::AnyHost, None, false)
            .expect("start the proxy");
        let local = connect_response(ssh.port(), &local_target);
        assert!(local.starts_with("HTTP/1.1 200"), "{local}");
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
}
