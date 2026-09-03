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
use std::ffi::{CString, OsString};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::REMOTE_SANDBOX_HELPER_FLAG;
use crate::RemoteSandboxNetwork;
use crate::linux_bubblewrap::install_command_seccomp_filter;
use crate::util::CanonicalPathBuf;

/// Exit code for "the sandbox could not be built" (mirrors the Bubblewrap
/// backend). The client keys its Japanese fail-closed error off this.
const SANDBOX_SETUP_FAILED_EXIT_CODE: i32 = 126;

/// Basename of the bundled slirp4netns binary installed next to the remote
/// server binary in `~/.zed_server`.
const SLIRP_BINARY_NAME: &str = "slirp4netns";

// ===========================================================================
// Entry point
// ===========================================================================

/// Handle a possible re-exec of this binary as the remote sandbox helper.
/// Neither stage returns if it matched. Called from
/// [`crate::run_remote_sandbox_helper_if_invoked`] at the top of `main`.
pub(crate) fn run_if_invoked() {
    let mut args = std::env::args_os();
    let _program = args.next();
    let Some(flag) = args.next() else {
        return;
    };
    if flag != REMOTE_SANDBOX_HELPER_FLAG {
        return;
    }
    let invocation = match parse_stage1_args(args) {
        Ok(invocation) => invocation,
        Err(error) => fail_closed("bad-args", &format!("{error:#}")),
    };
    run_helper(invocation);
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
    eprintln!("zed-remote-sandbox: {code}: {detail}");
    std::process::exit(SANDBOX_SETUP_FAILED_EXIT_CODE);
}

// ===========================================================================
// Stage 1 (parent): decode, capabilities, fork holder, maps, slirp, proxy
// ===========================================================================

/// A decoded stage-1 invocation.
struct Stage1Invocation {
    network: RemoteSandboxNetwork,
    allow_fs_write: bool,
    writable_paths: Vec<PathBuf>,
    protected_paths: Vec<PathBuf>,
    chdir: Option<PathBuf>,
    program: OsString,
    args: Vec<OsString>,
}

fn parse_stage1_args(args: std::env::ArgsOs) -> Result<Stage1Invocation> {
    let mut net_mode: Option<String> = None;
    let mut allowed_hosts: Vec<String> = Vec::new();
    let mut allow_fs_write = false;
    let mut writable_paths = Vec::new();
    let mut protected_paths = Vec::new();
    let mut chdir = None;
    let mut program = None;
    let mut trailing = Vec::new();

    let mut args = args;
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
    let program = program.context("missing program after `--`")?;

    Ok(Stage1Invocation {
        network,
        allow_fs_write,
        writable_paths,
        protected_paths,
        chdir,
        program,
        args: trailing,
    })
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
    let fs_backend = match select_fs_backend() {
        Ok(backend) => backend,
        Err(error) => fail_closed("no-fs-backend", &format!("{error:#}")),
    };

    // Add the protected paths the client cannot know about (see
    // `BUILTIN_PROTECTED_HOME_RELATIVE`). Resolved here in the parent, where the
    // environment is still the user's ssh session's, and before the fork so the
    // holder pins them along with the client's `.git` list.
    let Some(home) = remote_home_dir() else {
        fail_closed(
            "home-unknown",
            "neither $HOME nor the passwd database names an absolute home directory, \
             so the paths that must always be protected cannot be determined",
        );
    };
    invocation.protected_paths = merge_builtin_protected_paths(invocation.protected_paths, &home);
    log::info!(
        "[remote-sandbox] protecting {} paths ({} built-ins under HOME {})",
        invocation.protected_paths.len(),
        BUILTIN_PROTECTED_HOME_RELATIVE.len(),
        home.display()
    );

    let self_exe = std::env::current_exe().unwrap_or_default();
    let network_on = network_requested(&invocation.network);
    let slirp_path = locate_slirp4netns(&self_exe);
    if network_on && slirp_path.is_none() {
        fail_closed(
            "slirp-missing",
            &format!("{SLIRP_BINARY_NAME} was not found next to the remote server binary"),
        );
    }

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
            invocation,
            fs_backend,
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
        match start_egress_proxy(&invocation.network) {
            Ok(proxy) => Some(proxy),
            Err(error) => abort_holder(child_pid, "proxy-failed", &format!("{error:#}")),
        }
    } else {
        None
    };
    let proxy_port = proxy.as_ref().map(|proxy| proxy.port()).unwrap_or(0);

    let mut slirp_child = if network_on {
        match slirp_path
            .as_deref()
            .map(|path| start_slirp(path, child_pid))
            .transpose()
        {
            Ok(child) => child,
            Err(error) => abort_holder(child_pid, "slirp-failed", &format!("{error:#}")),
        }
    } else {
        None
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
                if let Some(slirp) = slirp_child.as_mut() {
                    let _ = slirp.kill();
                    let _ = slirp.wait();
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

    // Tear down slirp and the proxy now the command has finished.
    if let Some(slirp) = slirp_child.as_mut() {
        let _ = slirp.kill();
        let _ = slirp.wait();
    }
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
    let _ = wait_pid(child_pid);
    remove_command_temp_dirs(child_pid);
    fail_closed(code, detail)
}

const HOLDER_OK: u8 = 1;

/// The forked holder. Runs after the parent has mapped its ids and stood up the
/// network. It still holds full capabilities inside its own user namespace (it
/// never `exec`s a helper stage that would drop them), so it can build the mount
/// jail directly before finally `exec`ing the untrusted command. Never returns.
fn run_holder(
    invocation: Stage1Invocation,
    fs_backend: FsBackend,
    ready_write_fd: RawFd,
    go_read_fd: RawFd,
    relay_socket_fd: RawFd,
) -> ! {
    match run_holder_inner(
        invocation,
        fs_backend,
        ready_write_fd,
        go_read_fd,
        relay_socket_fd,
    ) {
        Ok(never) => match never {},
        Err((code, error)) => fail_closed(code, &format!("{error:#}")),
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              `CommandExt::exec` of the untrusted command is exactly what it must do"
)]
fn run_holder_inner(
    invocation: Stage1Invocation,
    fs_backend: FsBackend,
    ready_write_fd: RawFd,
    go_read_fd: RawFd,
    relay_socket_fd: RawFd,
) -> std::result::Result<std::convert::Infallible, (&'static str, anyhow::Error)> {
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

    // Pin every protected inode up front, for both backends, failing closed if
    // one exists but cannot be pinned (see `pin_protected_paths`).
    let protected_pins = pin_protected_paths(&invocation.protected_paths)
        .map_err(|e| ("protected-pin-failed", e))?;

    // Apply the filesystem backend. Both pin inodes by fd and fail closed. Each
    // yields the directory the command's `TMPDIR` should point at: a private
    // per-command dir under Landlock (no mount to isolate `/tmp`), or the
    // mount-jail's own private tmpfs at `/tmp`.
    let tmpdir: PathBuf = match fs_backend {
        FsBackend::Landlock(abi) => {
            let private_tmp = create_private_tmpdir().map_err(|e| ("mkdtemp-failed", e))?;
            apply_landlock(
                abi,
                &invocation.writable_paths,
                &protected_pins,
                invocation.allow_fs_write,
                &private_tmp,
            )
            .map_err(|e| ("landlock-failed", e))?;
            private_tmp
        }
        FsBackend::MountJail => {
            apply_mount_jail(
                &invocation.writable_paths,
                &protected_pins,
                invocation.allow_fs_write,
            )
            .map_err(|e| ("mount-failed", e))?;
            PathBuf::from("/tmp")
        }
    };

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
    let _ = inbound_writer.shutdown(Shutdown::Write);
    let _ = to_proxy.join();
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
/// `~/.zed_server` directory), or on `PATH` as a fallback.
fn locate_slirp4netns(self_exe: &Path) -> Option<PathBuf> {
    if let Some(dir) = self_exe.parent() {
        let candidate = dir.join(SLIRP_BINARY_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(SLIRP_BINARY_NAME))
        .find(|candidate| candidate.is_file())
}

/// Start `slirp4netns` attached to the holder's network namespace, and block
/// until it signals (via `--ready-fd`) that the interface is configured.
#[allow(
    clippy::disallowed_methods,
    reason = "the remote sandbox helper is a standalone process, not the GPUI app; \
              blocking process APIs are correct here"
)]
fn start_slirp(slirp_path: &Path, holder_pid: libc::pid_t) -> Result<std::process::Child> {
    let ready = Pipe::new().context("creating slirp ready pipe")?;
    let ready_write_raw = ready.write.as_raw_fd();
    let ready_read_raw = ready.read.as_raw_fd();

    let mut command = Command::new(slirp_path);
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

    let child = command.spawn().context("spawning slirp4netns")?;
    // Close our copy of the write end so EOF is observable if slirp dies.
    drop(ready.write);
    // Block until slirp writes its readiness byte (or closes the pipe on error).
    let _ = read_one_byte(ready_read_raw);
    let _ = ready_read_raw; // read end owned by `ready.read`, dropped here.
    drop(ready.read);
    Ok(child)
}

/// Stand up the host-loopback egress proxy enforcing the per-command allowlist.
fn start_egress_proxy(network: &RemoteSandboxNetwork) -> Result<http_proxy::ProxyHandle> {
    use http_proxy::{Allowlist, HostPattern, ProxyConfig, ProxyHandle};

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
        policy: None,
        // No editor-wide ceiling exists on the remote; the per-command allowlist
        // is the whole policy (documented boundary in the ADR self-audit).
        outer_policy: None,
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
fn select_fs_backend() -> Result<FsBackend> {
    let abi = landlock_abi();
    if abi >= 1 {
        return Ok(FsBackend::Landlock(abi));
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

/// `struct landlock_ruleset_attr`. Only the first (ABI-1) field is set; the
/// syscall is given `size_of` this, so the kernel zero-fills any later fields
/// (`handled_access_net`, …) it knows about — working on every ABI.
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
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

/// Paths — relative to the remote account's `$HOME` — that are protected on
/// **every** run, whatever the client asked for.
///
/// The client only knows to protect the project's `.git` locations
/// (`agent::sandboxing::sandbox_git_dirs`), which silently assumes the
/// worktree does not *contain* the account's own secrets. It regularly does: the
/// saved SSH connection in this fork opens `/home/<user>` itself, and then
/// everything below is a writable worktree — `~/.ssh/authorized_keys` (grant
/// yourself a login), `~/.bashrc` (run on the next login), `~/.zed_server/…`
/// (overwrite the remote server binary and get code execution on the next
/// connect), `~/.local/bin` and `~/.cargo/bin` (binaries already on `PATH`),
/// plus every credential store. A sandbox that blocks the network but lets the
/// command write those is not a sandbox, so the *helper* — the one component
/// that always runs on the host and knows its real `$HOME` — adds them itself
/// rather than trusting the client's list.
///
/// Semantics: **readable, never writable/creatable/deletable**. Entries that do
/// not exist are skipped (they cannot be pinned and there is nothing to
/// protect); an entry that exists but cannot be pinned fails the command closed
/// (see [`pin_protected_paths`]). Both directories and plain files appear here —
/// the rule layer narrows a non-directory's Landlock rights for exactly this
/// reason.
///
/// `/etc` and the rest of the system directories need no entry: they are outside
/// any worktree, and neither backend grants write outside the worktree
/// (Landlock never emits a rule for them; the mount jail's root is read-only).
const BUILTIN_PROTECTED_HOME_RELATIVE: &[&str] = &[
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
    // This fork's own remote install: the server binary the next connection
    // executes, and the state directory beside it.
    ".zed_server",
    ".local/share/zed-jp-enhance",
    // Binaries already on `PATH` for the next interactive shell.
    ".cargo/bin",
    ".local/bin",
    // Shell init: written once, executed on every later login.
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".config/fish",
    ".bash_logout",
];

/// The remote account's home directory: `$HOME` when it is absolute (the helper
/// is launched from the user's own ssh session, so it is set), otherwise the
/// passwd database.
fn remote_home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return Some(home);
        }
    }
    home_dir_from_passwd()
}

/// `getpwuid(getuid())->pw_dir`, for the case where `$HOME` is unset or
/// relative.
fn home_dir_from_passwd() -> Option<PathBuf> {
    // SAFETY: `getpwuid` takes a scalar and returns a pointer into a static
    // buffer that stays valid until the next passwd call; we copy out of it
    // immediately and make no other passwd call. The helper is single-threaded
    // at this point, so the non-`_r` variant is sound.
    let entry = unsafe { libc::getpwuid(libc::getuid()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: non-null passwd entry from the call above; `pw_dir` is either NULL
    // or a NUL-terminated string owned by that buffer.
    let dir = unsafe { (*entry).pw_dir };
    if dir.is_null() {
        return None;
    }
    // SAFETY: as above — a NUL-terminated C string valid for this read.
    let dir = unsafe { std::ffi::CStr::from_ptr(dir) };
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    home.is_absolute().then_some(home)
}

/// Resolve [`BUILTIN_PROTECTED_HOME_RELATIVE`] against `home`. A trailing
/// separator on `$HOME` is absorbed by `Path::join`, so `/home/u` and `/home/u/`
/// yield the same paths.
fn builtin_protected_paths(home: &Path) -> Vec<PathBuf> {
    BUILTIN_PROTECTED_HOME_RELATIVE
        .iter()
        .map(|relative| home.join(relative))
        .collect()
}

/// Append the built-in protected paths to the client's list, dropping ones the
/// client already named (which would otherwise be pinned twice).
fn merge_builtin_protected_paths(requested: Vec<PathBuf>, home: &Path) -> Vec<PathBuf> {
    let mut merged = requested;
    for builtin in builtin_protected_paths(home) {
        if !merged.contains(&builtin) {
            merged.push(builtin);
        }
    }
    merged
}

/// Pin every protected path, **failing closed** when one exists but cannot be
/// pinned.
///
/// A protected path that isn't there at all is skipped: there is no inode to
/// protect, so nothing is weakened by its absence. Every other failure aborts
/// the command, because a dropped protected pin is not a missing restriction —
/// it is a *granted* one. Under Landlock the protected pins are what tells
/// `grant_writable_subtree` to carve the worktree root up child by child;
/// with the pin silently discarded the root looked free of protected
/// descendants and received one recursive write grant, which made
/// `.git/PWNED` creatable (a `chmod 0300 .git` was enough to trigger it before
/// `O_PATH` was restored). Under the mount jail a dropped pin simply skips the
/// read-only re-bind.
fn pin_protected_paths(paths: &[PathBuf]) -> Result<Vec<CanonicalPathBuf>> {
    let mut pins = Vec::with_capacity(paths.len());
    for path in paths {
        match pin_path(path) {
            Ok(pin) => pins.push(pin),
            Err(error) => {
                // Only a genuine "it isn't there" is a skip. Anything else —
                // notably an unsearchable parent, which reports
                // `PermissionDenied` here too — means we cannot tell what is at
                // that path, so it must not be treated as absent.
                //
                //
                // A **dangling symlink** is therefore *not* absent: the entry is
                // there, only its target isn't, and a write through it creates
                // that target — which for `~/.bashrc -> dotfiles/bashrc` inside
                // a writable home directory would be executed at the next login.
                // The built-in set makes this reachable in a way the `.git`-only
                // list never was, so it stays fail-closed (loud, and the failure
                // detail names the path) rather than silently unprotected.
                let missing = std::fs::symlink_metadata(path)
                    .err()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
                if missing {
                    continue;
                }
                let reason = error.root_cause().to_string();
                bail!("{}: {reason}", path.display());
            }
        }
    }
    Ok(pins)
}

fn apply_landlock(
    abi: i32,
    writable_paths: &[PathBuf],
    protected_pins: &[CanonicalPathBuf],
    allow_fs_write: bool,
    private_tmp: &Path,
) -> Result<()> {
    // No-new-privs is a precondition for `landlock_restrict_self`.
    set_no_new_privs()?;

    let handled = landlock_handled_write_access(abi);
    let attr = LandlockRulesetAttr {
        handled_access_fs: handled,
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

    if allow_fs_write {
        // Unrestricted writes: allow the whole root — but still carved around
        // the protected paths, exactly as a worktree root is. Landlock is
        // additive-allow, so "everything except `~/.ssh`" can only be expressed
        // by granting `/`'s entries one level at a time down to each protected
        // path (a handful of directories: `/`, `/home`, `$HOME`, and the few
        // `$HOME` subdirectories that contain one). This matches the mount jail,
        // which re-binds the protected paths read-only under `allow_fs_write`
        // too — turning off the write restriction was never meant to hand out
        // the account's ssh keys.
        grant_writable_subtree(
            ruleset.as_raw_fd(),
            Path::new("/"),
            protected_pins,
            handled,
            abi,
        )
        .context("allowing write on / for allow_fs_write")?;
    } else {
        for writable in writable_paths {
            grant_writable_subtree(ruleset.as_raw_fd(), writable, protected_pins, handled, abi)?;
        }
    }

    // Always allow the standard ephemeral write locations. These are character
    // devices (non-directories), so `add_path_rule` narrows the granted access
    // to the file-type subset for them (a directory-only access here would be
    // rejected with `EINVAL`, just as it was for the writable-root children).
    for dev in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/tty",
        "/dev/random",
        "/dev/urandom",
    ] {
        // Best-effort: a missing node is simply skipped.
        let _ = add_path_rule(ruleset.as_raw_fd(), Path::new(dev), handled, abi);
    }
    // A **private** per-command temp directory (not all of `/tmp`): granting `/tmp`
    // wholesale would recursively re-expose any writable/protected path that
    // happens to live under it (e.g. a `.git` inside a `/tmp` worktree). The
    // command's `TMPDIR` is pointed here by the caller (mount-jail gives a
    // private tmpfs at `/tmp` instead).
    add_path_rule(ruleset.as_raw_fd(), private_tmp, handled, abi).with_context(|| {
        format!(
            "allowing write on private temp dir {}",
            private_tmp.display()
        )
    })?;

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

/// Grant write on a writable root, carving out every protected path inside it
/// (the project's `.git`, and the built-in `$HOME` set when the worktree *is*
/// the home directory): rather than a single recursive rule on the root — which
/// Landlock's additive-allow model could never re-restrict — the root's entries
/// are enumerated and granted one by one, skipping the protected ones and
/// descending into any entry that merely *contains* one.
///
/// The cost is that creating a brand-new entry *directly* in a carved directory
/// is denied (existing entries, and everything under them, stay writable). It is
/// unavoidable: `MAKE_REG` on a directory is inherited by every directory below
/// it, so a rule that lets the command create `~/newfile` necessarily lets it
/// create `~/.ssh/authorized_keys` too. When a directory has no protected
/// descendant it is granted wholesale and keeps full behavior — which is still
/// the common case, since a project below `$HOME` never contains one of these.
fn grant_writable_subtree(
    ruleset_fd: RawFd,
    writable: &Path,
    protected_pins: &[CanonicalPathBuf],
    handled: u64,
    abi: i32,
) -> Result<()> {
    let writable_pin = match pin_path(writable) {
        Ok(pin) => pin,
        // A writable path that no longer exists is skipped (fail-closed: it
        // simply isn't granted) rather than aborting the whole command.
        Err(error) => {
            log::warn!(
                "[remote-sandbox] skipping writable path {}: {error:#}",
                writable.display()
            );
            return Ok(());
        }
    };
    grant_subtree_excluding_protected(ruleset_fd, &writable_pin, protected_pins, handled, abi, 0)
}

/// One level of [`grant_writable_subtree`]'s carve. `dir_pin` is the directory
/// being granted or carved; `depth` is 0 for the writable root itself.
fn grant_subtree_excluding_protected(
    ruleset_fd: RawFd,
    dir_pin: &CanonicalPathBuf,
    protected_pins: &[CanonicalPathBuf],
    handled: u64,
    abi: i32,
    depth: usize,
) -> Result<()> {
    let dir = dir_pin.path();
    // The directory itself is protected: grant nothing. The recursion never
    // descends into a protected entry, so this only fires when the *writable
    // root* is one — in which case protection wins over the write grant.
    if protected_pins.iter().any(|protected| protected == dir_pin) {
        log::warn!(
            "[remote-sandbox] granting no write on {}: it is a protected path",
            dir.display()
        );
        return Ok(());
    }
    // A protected path *strictly inside* this directory forces the carve; one
    // that *is* this directory was just handled above.
    let has_protected_descendant = protected_pins.iter().any(|protected| {
        let protected = protected.path();
        protected.starts_with(dir) && protected != dir
    });
    if !has_protected_descendant {
        return add_path_rule(ruleset_fd, dir, handled, abi)
            .with_context(|| format!("allowing write on {}", dir.display()));
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
        if protected_pins
            .iter()
            .any(|protected| protected == &child_pin)
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
        // An entry that contains a protected path is carved in turn instead of
        // being granted wholesale — this is what keeps `~/.config/gh` and
        // `~/.local/share/zed-jp-enhance` protected while `~/.config` and
        // `~/.local` are inside a writable home directory.
        if protected_pins
            .iter()
            .any(|protected| protected.path().starts_with(child_pin.path()))
        {
            grant_subtree_excluding_protected(
                ruleset_fd,
                &child_pin,
                protected_pins,
                handled,
                abi,
                depth + 1,
            )?;
            continue;
        }
        // Grant write on the child. `add_path_rule` reads the child's inode type
        // and narrows the access accordingly: a plain file (e.g. `file.txt`)
        // beside the protected `.git` gets only the file-type accesses, so the
        // directory-only bits in `handled` no longer trip `EINVAL`.
        //
        // One entry that cannot take a rule must not fail the whole command
        // closed (that is how the `EINVAL` bug above blocked every command on
        // 24.04): it is skipped, which denies writes there and nothing else.
        if let Err(error) = add_path_rule(ruleset_fd, child_pin.path(), handled, abi) {
            log::warn!(
                "[remote-sandbox] not granting write on {}: {error:#}",
                child_pin.path().display()
            );
        }
    }
    Ok(())
}

/// Add a `LANDLOCK_RULE_PATH_BENEATH` rule granting write beneath `path`,
/// pinning the inode by an `O_PATH` fd (so no path is re-resolved at enforcement
/// time). The exact access is chosen from the pinned inode's type via
/// [`landlock_entry_write_access`]: a directory receives the full handled write
/// set, a non-directory only file-type accesses — otherwise Landlock rejects the
/// rule with `EINVAL`. `handled` is the ruleset's handled mask and `abi` the
/// detected Landlock ABI (both bound the accesses that can be requested).
fn add_path_rule(ruleset_fd: RawFd, path: &Path, handled: u64, abi: i32) -> Result<()> {
    // `open_o_path` rather than `OpenOptions::custom_flags`: std masks custom
    // flags with `!O_ACCMODE`, and musl's `O_ACCMODE` contains `O_PATH` — see
    // the note on `open_o_path`. Without it this became a real read-open, which
    // blocks forever on a FIFO sitting in the worktree root this enumerates.
    let fd = crate::util::open_o_path(path, crate::util::FollowSymlinks::Yes)
        .with_context(|| format!("opening {}", path.display()))?;
    let is_dir = fd_is_dir(fd.as_raw_fd())
        .with_context(|| format!("determining inode type of {}", path.display()))?;
    let access = landlock_entry_write_access(is_dir, abi, handled);
    let attr = LandlockPathBeneathAttr {
        allowed_access: access,
        parent_fd: fd.as_raw_fd(),
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
// Mount jail (older kernels without Landlock, e.g. 18.04 / kernel 5.4)
// ===========================================================================

/// Build a read-only view of `/` with a read-write worktree (and protected
/// paths re-bound read-only over it), then `pivot_root` into it and detach the
/// old root. Requires a fresh mount namespace, which we can create because we
/// are inside our own user namespace.
fn apply_mount_jail(
    writable_paths: &[PathBuf],
    protected_pins: &[CanonicalPathBuf],
    allow_fs_write: bool,
) -> Result<()> {
    // Pin the writable inodes before mounting so we can verify the bind wasn't
    // redirected afterwards.
    let writable_pins: Vec<CanonicalPathBuf> = writable_paths
        .iter()
        .filter_map(|path| pin_path(path).ok())
        .collect();

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
    let _ = std::fs::create_dir_all(&jail_tmp);
    mount(Some(Path::new("tmpfs")), &jail_tmp, Some("tmpfs"), 0, None)
        .context("mounting private /tmp")?;

    // Re-bind each writable path read-write over the RO root.
    for pin in &writable_pins {
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
    let _ = std::fs::remove_dir("/tmp/.oldroot");
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
    let expected = crate::util::linux_fd_identity(pin.fd().as_raw_fd())
        .context("fstat of pinned writable inode")?;
    let actual = (mounted_meta.dev(), mounted_meta.ino());
    if expected != actual {
        bail!(
            "writable bind {} was redirected (pinned {:?}, mounted {:?})",
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

        // Prepend a fake argv0 and the helper flag is already argv[0] of the
        // slice we pass to `parse_stage1_args` (which expects args *after* the
        // flag), so drop the leading `--remote-sandbox-helper`.
        assert_eq!(argv[0], REMOTE_SANDBOX_HELPER_FLAG);
        let decoded = decode_for_test(&argv[1..]);

        assert_eq!(
            decoded.network,
            RemoteSandboxNetwork::Hosts(vec!["example.com".into()])
        );
        assert!(!decoded.allow_fs_write);
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

    /// The stage-1 decoder consumes `std::env::ArgsOs`, which tests can't
    /// synthesize; this mirror parses a `&[String]` with the identical grammar.
    fn decode_for_test(args: &[String]) -> Stage1Invocation {
        let mut net_mode: Option<String> = None;
        let mut allowed_hosts: Vec<String> = Vec::new();
        let mut allow_fs_write = false;
        let mut writable_paths = Vec::new();
        let mut protected_paths = Vec::new();
        let mut chdir = None;
        let mut program = None;
        let mut trailing = Vec::new();

        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            match flag.as_str() {
                "--net" => net_mode = Some(iter.next().unwrap().clone()),
                "--allow-host" => allowed_hosts.push(iter.next().unwrap().clone()),
                "--allow-fs-write" => allow_fs_write = true,
                "--writable" => writable_paths.push(PathBuf::from(iter.next().unwrap())),
                "--protected" => protected_paths.push(PathBuf::from(iter.next().unwrap())),
                "--chdir" => chdir = Some(PathBuf::from(iter.next().unwrap())),
                "--" => {
                    program = iter.next().map(OsString::from);
                    trailing = iter.map(OsString::from).collect();
                    break;
                }
                other => panic!("unexpected flag {other}"),
            }
        }
        let network = match net_mode.as_deref() {
            Some("blocked") | None => RemoteSandboxNetwork::Blocked,
            Some("any") => RemoteSandboxNetwork::AnyHost,
            Some("hosts") => RemoteSandboxNetwork::Hosts(allowed_hosts),
            Some(other) => panic!("unknown net {other}"),
        };
        Stage1Invocation {
            network,
            allow_fs_write,
            writable_paths,
            protected_paths,
            chdir,
            program: program.unwrap(),
            args: trailing,
        }
    }

    /// Create a Landlock ruleset for a test. Creating one only allocates an fd;
    /// nothing is enforced until `landlock_restrict_self`, which no test calls
    /// (it is irreversible and would restrict the whole test process).
    fn create_test_ruleset(abi: i32) -> OwnedFd {
        let attr = LandlockRulesetAttr {
            handled_access_fs: landlock_handled_write_access(abi),
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

    /// Pin every direct child of `root` the way the enumeration does, for
    /// kernels with no Landlock to build a ruleset on.
    fn pin_every_child(root: &Path) -> Result<()> {
        for entry in std::fs::read_dir(root)? {
            pin_path(&entry?.path())?;
        }
        Ok(())
    }

    /// Regression: the writable-root enumeration opens every direct child, and
    /// an ordinary `open` of a FIFO with no writer blocks forever — so a single
    /// named pipe in a worktree root wedged the whole sandbox setup with no
    /// error, no exit and no timeout (it isn't even fail-closed; nothing ever
    /// happens). `O_PATH` never waits for a partner.
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

        let protected = pin_protected_paths(&[git]).expect("pinning .git");
        let root_path = root.path().to_path_buf();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let abi = landlock_abi();
            let outcome = if abi >= 1 {
                let ruleset = create_test_ruleset(abi);
                grant_writable_subtree(
                    ruleset.as_raw_fd(),
                    &root_path,
                    &protected,
                    landlock_handled_write_access(abi),
                    abi,
                )
            } else {
                pin_every_child(&root_path)
            };
            let _ = tx.send(outcome.is_ok());
        });
        let granted = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("granting a writable root containing a FIFO must not block");
        assert!(granted, "a FIFO in the root must not fail the grant either");
    }

    /// A protected path that cannot be pinned must abort the command, never be
    /// dropped: under Landlock a missing protected pin makes the worktree root
    /// look free of protected descendants, and it is then granted write
    /// *recursively* — which is how `.git/PWNED` became creatable.
    #[test]
    fn an_unpinnable_protected_path_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("tempdir");

        // Absent: nothing to protect, so nothing is weakened by skipping it.
        let absent = root.path().join("nowhere/.git");
        assert!(
            pin_protected_paths(&[absent])
                .expect("an absent protected path is skipped")
                .is_empty()
        );

        // A dangling symlink is *not* absent: the entry exists, and a write
        // through it would create its target (a `~/.bashrc -> dotfiles/bashrc`
        // inside a writable home directory then runs at the next login). We
        // cannot pin what it points at, so the command must not run.
        let dangling = root.path().join("dangling");
        std::os::unix::fs::symlink(root.path().join("nowhere"), &dangling).expect("symlink");
        assert!(
            pin_protected_paths(&[dangling]).is_err(),
            "a dangling protected symlink must fail closed"
        );

        // Present but unreachable (its parent cannot be searched): we cannot
        // tell what is there, so the command must not run.
        let closed = root.path().join("closed");
        std::fs::create_dir(&closed).expect("create closed");
        std::fs::create_dir(closed.join(".git")).expect("create .git");
        std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o000))
            .expect("chmod 0000");
        let result = pin_protected_paths(&[closed.join(".git")]);
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

        let pins = pin_protected_paths(std::slice::from_ref(&git));
        // Restore the mode before asserting, so the temp dir can clean itself up
        // (its recursive delete has to read this directory) either way.
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).expect("restore");
        assert_eq!(
            pins.expect("a 0300 directory must still pin").len(),
            1,
            "the protected path must be pinned, not skipped"
        );
    }

    /// The built-in set is resolved against `$HOME`, including the multi-segment
    /// and file entries, and a trailing separator on `$HOME` changes nothing.
    #[test]
    fn builtin_protected_paths_resolve_against_home() {
        let paths = builtin_protected_paths(Path::new("/home/u"));
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
            builtin_protected_paths(Path::new("/home/u/")),
            paths,
            "a trailing separator on $HOME must not change the resolved paths"
        );
    }

    /// Built-in entries that don't exist are skipped rather than failing the
    /// command, and a **file** entry pins as a non-directory (so the rule layer
    /// narrows its Landlock rights instead of tripping `EINVAL`).
    #[test]
    fn builtin_protected_paths_pin_the_entries_that_exist() {
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(home.path().join(".ssh")).expect("create .ssh");
        std::fs::write(home.path().join(".bashrc"), "x").expect("write .bashrc");

        let pins = pin_protected_paths(&builtin_protected_paths(home.path()))
            .expect("absent built-in entries must be skipped, not fatal");
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
        let merged = merge_builtin_protected_paths(
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
        assert_eq!(merged.len(), 1 + BUILTIN_PROTECTED_HOME_RELATIVE.len());
    }

    /// Landlock end-to-end for the shape the built-in set creates: with the
    /// worktree *being* the home directory, a protected **file** (`.bashrc`) and
    /// a protected **nested** directory (`.config/gh`) must be unwritable while
    /// their siblings stay writable, and a symlink pointing out of the worktree
    /// must not smuggle a write grant to its target.
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

        let protected =
            pin_protected_paths(&[root.path().join(".bashrc"), root.path().join(".config/gh")])
                .expect("pinning the protected entries");
        let writable = vec![root.path().to_path_buf()];
        let root_path = root.path().to_path_buf();
        let tmp_path = private_tmp.path().to_path_buf();

        let results = std::thread::spawn(move || -> Result<Vec<(&'static str, bool)>> {
            apply_landlock(abi, &writable, &protected, false, &tmp_path)?;
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
                ("escape/new.txt", create(root_path.join("escape/new.txt"))),
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
