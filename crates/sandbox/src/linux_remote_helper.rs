//! ADR-F18: the on-host sandbox helper that physically isolates an SSH-remote
//! Linux agent command.
//!
//! Bubblewrap cannot be used on Ubuntu 24.04 (`apparmor_restrict_unprivileged_userns=1`
//! refuses both the unprivileged-userns `uid_map` self-write and unprivileged
//! `mount`), so this backend builds the isolation by hand out of primitives that
//! *do* work there, verified on real 24.04 and 18.04 hosts:
//!
//! - **Network**: a `CLONE_NEWUSER|CLONE_NEWNET` "holder" whose only outbound
//!   path is `slirp4netns --outbound-addr=127.0.0.1` (external NAT dies with
//!   "Network is unreachable"; only the built-in host-loopback forwarding
//!   `guest -> 10.0.2.2 -> host 127.0.0.1` survives). A host-loopback egress
//!   proxy (`http_proxy`) on `127.0.0.1:<port>` enforces the per-command host
//!   allowlist; the command's env points `HTTP(S)_PROXY` at `10.0.2.2:<port>`.
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

/// slirp4netns fixed addressing (its `--configure` defaults). The gateway
/// `10.0.2.2` is libslirp's host-loopback alias, which is the only reachable
/// destination once `--outbound-addr=127.0.0.1` has killed external NAT.
const SLIRP_GATEWAY: &str = "10.0.2.2";
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
fn run_helper(invocation: Stage1Invocation) -> ! {
    // --- Capability gate (fail-closed before running anything) -------------
    let fs_backend = match select_fs_backend() {
        Ok(backend) => backend,
        Err(error) => fail_closed("no-fs-backend", &format!("{error:#}")),
    };

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
    let ready_write_fd = ready.write.as_raw_fd();
    let go_read_fd = go.read.as_raw_fd();

    // SAFETY: a single `fork` from a single-threaded process. The holder becomes
    // its own process; it and the parent coordinate only through the pipes.
    let child_pid = unsafe { libc::fork() };
    if child_pid < 0 {
        fail_closed("fork-failed", "fork() failed");
    }
    if child_pid == 0 {
        // Holder. Never returns. (`invocation` and `fs_backend` are moved in only
        // on this diverging path, so the parent below still owns them.)
        run_holder(invocation, fs_backend, ready_write_fd, go_read_fd);
    }

    // --- Parent -----------------------------------------------------------
    // Close the ends we don't use so the holder's copies become the sole
    // remaining ones (EOF semantics if it dies).
    drop(ready.write);
    drop(go.read);
    let ready_read = ready.read;
    let go_write = go.write;

    // Wait for the holder to report it created the namespaces (a non-OK byte or
    // EOF means it couldn't).
    match read_one_byte(ready_read.as_raw_fd()) {
        Ok(HOLDER_OK) => {}
        _ => {
            let _ = wait_pid(child_pid);
            fail_closed(
                "userns-failed",
                "the remote host refused to create a user+network namespace \
                 (unprivileged user namespaces may be disabled)",
            );
        }
    }

    // Map the holder's uid/gid to our real ids so worktree files (owned by us)
    // stay writable and the command doesn't run as `nobody`.
    if let Err(error) = write_id_maps(child_pid) {
        let _ = wait_pid(child_pid);
        fail_closed("uidmap-failed", &format!("{error:#}"));
    }

    // Stand up the egress proxy (host loopback) now — after the fork, so its
    // threads never entered the holder — then attach slirp to the holder's
    // network namespace.
    let proxy = if network_on {
        match start_egress_proxy(&invocation.network) {
            Ok(proxy) => Some(proxy),
            Err(error) => {
                let _ = wait_pid(child_pid);
                fail_closed("proxy-failed", &format!("{error:#}"));
            }
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
            Err(error) => {
                let _ = wait_pid(child_pid);
                fail_closed("slirp-failed", &format!("{error:#}"));
            }
        }
    } else {
        None
    };

    // Release the holder, handing it the proxy port (0 = no proxy).
    if let Err(error) = write_all_bytes(go_write.as_raw_fd(), &proxy_port.to_le_bytes()) {
        let _ = wait_pid(child_pid);
        fail_closed("handshake-failed", &error.to_string());
    }
    drop(go_write);

    let status = wait_pid(child_pid);

    // Tear down slirp and the proxy now the command has finished.
    if let Some(slirp) = slirp_child.as_mut() {
        let _ = slirp.kill();
        let _ = slirp.wait();
    }
    drop(proxy);

    match status {
        Ok(code) => std::process::exit(code),
        Err(_) => fail_closed("wait-failed", "failed to wait for the sandboxed command"),
    }
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
) -> ! {
    match run_holder_inner(invocation, fs_backend, ready_write_fd, go_read_fd) {
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
                &invocation.protected_paths,
                invocation.allow_fs_write,
                &private_tmp,
            )
            .map_err(|e| ("landlock-failed", e))?;
            private_tmp
        }
        FsBackend::MountJail => {
            apply_mount_jail(
                &invocation.writable_paths,
                &invocation.protected_paths,
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
    // Point the command's proxy env at slirp's host-loopback gateway.
    if proxy_port != 0 {
        let url = format!("http://{SLIRP_GATEWAY}:{proxy_port}");
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
        // ("Network is unreachable"), leaving only the host-loopback forwarding
        // to the proxy reachable.
        .arg("--outbound-addr=127.0.0.1")
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

fn apply_landlock(
    abi: i32,
    writable_paths: &[PathBuf],
    protected_paths: &[PathBuf],
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

    // Pin protected inodes so writable subtrees can be carved around them.
    let protected_pins: Vec<CanonicalPathBuf> = protected_paths
        .iter()
        .filter_map(|path| pin_path(path).ok())
        .collect();

    if allow_fs_write {
        // Unrestricted writes: allow the whole root. Landlock is additive-allow,
        // so protected paths under `/` cannot be re-restricted here (mount-jail
        // does protect them); this matches the documented Landlock trade-off.
        add_path_rule(ruleset.as_raw_fd(), Path::new("/"), handled, abi)
            .context("allowing write on / for allow_fs_write")?;
    } else {
        for writable in writable_paths {
            grant_writable_subtree(ruleset.as_raw_fd(), writable, &protected_pins, handled, abi)?;
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

/// Grant write on a writable root, carving out any protected path that is a
/// *direct child* of it (notably `.git`): rather than a single recursive rule on
/// the root — which Landlock's additive-allow model could never re-restrict — we
/// enumerate the root's entries and grant each one except the protected ones.
///
/// The cost is that creating a brand-new entry *directly* in the root is denied
/// (existing entries, and everything under them, stay writable). When the root
/// has no protected descendant we grant it wholesale and keep full behavior.
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

    // Does any protected path sit inside this writable root?
    let has_protected_descendant = protected_pins
        .iter()
        .any(|protected| protected.path().starts_with(writable_pin.path()));

    if !has_protected_descendant {
        add_path_rule(ruleset_fd, writable_pin.path(), handled, abi)
            .with_context(|| format!("allowing write on {}", writable_pin.path().display()))?;
        return Ok(());
    }

    // Enumerate direct children and grant each non-protected one.
    let entries = std::fs::read_dir(writable_pin.path())
        .with_context(|| format!("reading {}", writable_pin.path().display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", writable_pin.path().display()))?;
        let child = entry.path();
        let Ok(child_pin) = pin_path(&child) else {
            continue;
        };
        let is_protected = protected_pins
            .iter()
            .any(|protected| protected == &child_pin);
        if is_protected {
            continue;
        }
        // Grant write on the child. `add_path_rule` reads the child's inode type
        // and narrows the access accordingly: a plain file (e.g. `file.txt`)
        // beside the protected `.git` gets only the file-type accesses, so the
        // directory-only bits in `handled` no longer trip `EINVAL`.
        add_path_rule(ruleset_fd, child_pin.path(), handled, abi)
            .with_context(|| format!("allowing write on {}", child_pin.path().display()))?;
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
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let fd = OwnedFd::from(file);
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

/// Create a private, owner-only per-command temp directory (for the Landlock
/// backend's `TMPDIR`; mount-jail uses its own private tmpfs at `/tmp` instead).
fn create_private_tmpdir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt as _;
    let dir = PathBuf::from(format!("/tmp/zed-remote-sandbox-{}", std::process::id()));
    // Clear a stale dir left by a recycled pid; only succeeds for one we own.
    let _ = std::fs::remove_dir_all(&dir);
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
    protected_paths: &[PathBuf],
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
    let _ = std::fs::remove_dir_all(&new_root);
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
    for protected in protected_paths {
        let Ok(protected_pin) = pin_path(protected) else {
            continue;
        };
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
