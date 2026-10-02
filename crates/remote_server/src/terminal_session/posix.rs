//! The holder, `attach`, and `kill` on Linux and macOS.

use super::replay::OutputHistory;
use super::wire::{
    Decoder, PREAMBLE_LEN, PROTOCOL_VERSION, Role, ShellStatus, ToAttach, ToHolder, WindowSize,
    encode_preamble,
};
use super::{
    EXIT_DETACHED, EXIT_FAILURE, EXIT_TAKEN_OVER, SESSION_ENDED_REASON, TerminalSessionCommand,
    UNATTACHED_HOLDER_TIMEOUT, attach_failure_log, clamp_replay_bytes, fits_unix_socket_path,
    holder_detached_timeout, print_error, replay_begin_marker, replay_end_marker,
    resumed_without_shell_notice, session_file_stem, shell_exited_while_detached_notice,
    taken_over_notice,
};
use anyhow::{Context as _, Result, bail};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};
use util::ResultExt as _;

/// How long an attach waits for the holder it started to accept connections.
/// Longer than [`HOLDER_LOCK_TIMEOUT`], which that holder may spend waiting
/// for a holder of the same session that is still shutting down.
const HOLDER_START_TIMEOUT: Duration = Duration::from_secs(15);
const HOLDER_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const _: () = assert!(
    UNATTACHED_HOLDER_TIMEOUT.as_secs()
        > HOLDER_START_TIMEOUT.as_secs() + HANDSHAKE_TIMEOUT.as_secs()
);
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
const RETIRING_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
/// Between the hangup and the kill of a shell that is being ended.
const TERMINATION_GRACE: Duration = Duration::from_secs(3);
/// How long a holder waits for its shell to die after killing it.
const TERMINATION_GIVE_UP: Duration = Duration::from_secs(2);
/// How long a full-screen program sees the smaller size before the real one
/// comes back: two size changes at once can reach it as a single SIGWINCH
/// about a size that did not change.
const REDRAW_DELAY: Duration = Duration::from_millis(100);
/// While an attach has this much output still to take, the shell's output
/// is left in the pseudo-terminal, which makes the shell wait as it would on
/// a slow terminal. Reading on regardless would only move the backlog into
/// this process, and a link slower than the shell (`cat` of a large file)
/// would grow it without bound.
const OUTPUT_BACKLOG_LIMIT: usize = 1024 * 1024;
/// How long an attach with output to take may go without taking any before
/// it is let go: its connection is gone without having said so, and the
/// shell must not wait for it any longer.
///
/// Longer than the client's `ssh` takes to give up on a silent server by its
/// keepalive (`ServerAliveInterval=15`, `ServerAliveCountMax=3`: 45 s). Up to
/// then an outage can pass with the connection intact, and letting the attach
/// go would still end it (it finds the holder gone once its output is
/// through), showing a healthy terminal as disconnected. After that, a client
/// that has not taken anything has lost its `ssh`, and only the shell is
/// still waiting.
const STALLED_ATTACH_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UNWRITTEN_INPUT: usize = 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;

const LOG_FILE_NAME: &str = "terminal_sessions.log";
const LOG_MAX_BYTES: u64 = 1024 * 1024;

pub(super) fn run(command: TerminalSessionCommand) -> i32 {
    match command {
        TerminalSessionCommand::Attach {
            id,
            nonce,
            resume,
            detached_timeout_secs,
            replay_bytes,
            program,
        } => {
            init_logging(log::LevelFilter::Warn);
            attach(&AttachOptions {
                id,
                nonce,
                resume,
                detached_timeout_secs,
                replay_bytes: clamp_replay_bytes(replay_bytes),
                program,
            })
        }
        TerminalSessionCommand::Kill { id } => {
            init_logging(log::LevelFilter::Warn);
            match kill_session(&sessions_directory(), &id) {
                Ok(()) => 0,
                Err(error) => {
                    log::warn!("could not end terminal session {id}: {error:#}");
                    print_error(&format!("could not end the terminal session: {error:#}"));
                    EXIT_FAILURE
                }
            }
        }
        TerminalSessionCommand::Holder {
            id,
            detached_timeout_secs,
            replay_bytes,
            program,
        } => {
            init_logging(log::LevelFilter::Info);
            run_holder(HolderOptions {
                id,
                detached_timeout: (detached_timeout_secs > 0)
                    .then(|| Duration::from_secs(detached_timeout_secs)),
                replay_bytes: clamp_replay_bytes(replay_bytes),
                program,
            })
        }
    }
}

#[cfg(test)]
const TEST_DIRECTORY_VAR: &str = "ZED_TERMINAL_SESSION_TEST_DIRECTORY";

pub(super) fn sessions_directory() -> PathBuf {
    #[cfg(test)]
    if let Some(directory) = std::env::var_os(TEST_DIRECTORY_VAR) {
        return directory.into();
    }
    paths::remote_server_state_dir().join("terminal_sessions")
}

#[cfg(test)]
const TEST_STALLED_ATTACH_TIMEOUT_VAR: &str = "ZED_TERMINAL_SESSION_TEST_STALLED_ATTACH_MS";

fn stalled_attach_timeout() -> Duration {
    #[cfg(test)]
    if let Some(milliseconds) = std::env::var(TEST_STALLED_ATTACH_TIMEOUT_VAR)
        .ok()
        .and_then(|milliseconds| milliseconds.parse().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    STALLED_ATTACH_TIMEOUT
}

#[cfg(test)]
const TEST_UNATTACHED_HOLDER_TIMEOUT_VAR: &str = "ZED_TERMINAL_SESSION_TEST_UNATTACHED_HOLDER_MS";

fn unattached_holder_timeout() -> Duration {
    #[cfg(test)]
    if let Some(milliseconds) = std::env::var(TEST_UNATTACHED_HOLDER_TIMEOUT_VAR)
        .ok()
        .and_then(|milliseconds| milliseconds.parse().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    UNATTACHED_HOLDER_TIMEOUT
}

fn log_path() -> PathBuf {
    #[cfg(test)]
    if let Some(directory) = std::env::var_os(TEST_DIRECTORY_VAR) {
        return PathBuf::from(directory).join(LOG_FILE_NAME);
    }
    paths::logs_dir().join(LOG_FILE_NAME)
}

/// One log file for every holder and attach on the host, each record appended
/// with a single write, rotated like the server's own log.
struct SharedLogFile {
    path: PathBuf,
}

impl io::Write for SharedLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if std::fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() >= LOG_MAX_BYTES) {
            crate::rotate_log_file(&self.path, &crate::rotated_log_path(&self.path))?;
        }
        crate::open_log_file(&self.path)?.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn init_logging(level: log::LevelFilter) {
    let path = log_path();
    // A failure has nowhere to be told: stderr is /dev/null in a holder and
    // the user's terminal in an attach.
    if let Some(directory) = path.parent()
        && std::fs::create_dir_all(directory).is_err()
    {
        return;
    }
    let pid = std::process::id();
    env_logger::Builder::new()
        .filter_level(level)
        .format(move |buffer, record| {
            writeln!(
                buffer,
                "{} {:<5} [terminal-session {pid}] {}",
                buffer.timestamp(),
                record.level(),
                record.args()
            )
        })
        .target(env_logger::Target::Pipe(Box::new(SharedLogFile { path })))
        .try_init()
        .ok();
}

/// Where one session's socket and lock live.
struct SessionPaths {
    socket: PathBuf,
    lock: PathBuf,
}

impl SessionPaths {
    fn resolve(directory: &Path, id: &str, create: bool) -> Result<Self> {
        let stem = session_file_stem(&host_key(), id);
        let socket_name = format!("{stem}.sock");
        let mut directory = directory.to_path_buf();
        if !fits_unix_socket_path(&directory.join(&socket_name)) {
            // A home directory deep enough to overflow `sun_path` (an NFS
            // automount, say) still has a short temporary directory.
            directory = std::env::temp_dir().join(format!("zed-terminal-sessions-{}", this_user()));
            if !fits_unix_socket_path(&directory.join(&socket_name)) {
                bail!(
                    "no directory for terminal session sockets has a path short enough: {}",
                    directory.display()
                );
            }
        }
        if create {
            ensure_private_directory(&directory)?;
        }
        Ok(Self {
            socket: directory.join(socket_name),
            lock: directory.join(format!("{stem}.lock")),
        })
    }
}

/// The machine ID tells apart hosts that share a home directory. Not the host
/// name: that can change while a shell runs, and would orphan it.
fn host_key() -> String {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .map(|machine_id| machine_id.trim().to_string())
        .unwrap_or_default()
}

fn this_user() -> libc::uid_t {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn ensure_private_directory(directory: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("reading {}", directory.display()))?;
    // The fallback lives in the shared temporary directory, where someone
    // else could have made it first.
    if !metadata.is_dir() || metadata.uid() != this_user() {
        bail!("{} is not a directory of this user", directory.display());
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting {} to this user", directory.display()))?;
    }
    Ok(())
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

fn remove_if_identical(path: &Path, identity: (u64, u64)) {
    if file_identity(path) == Some(identity) {
        crate::remove_file_if_present(path).log_err();
    }
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(unused_variables)
)]
fn is_this_user(stream: &UnixStream) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match crate::daemon::peer_is_this_user(stream) {
        Ok(true) => true,
        Ok(false) => {
            log::warn!("refused a connection from a process of another user");
            false
        }
        Err(error) => {
            log::warn!("refused a connection whose owner could not be read: {error}");
            false
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        // The socket's directory is only this user's.
        true
    }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn set_flag(fd: RawFd, get: libc::c_int, set: libc::c_int, flag: libc::c_int) -> io::Result<()> {
    // SAFETY: `fcntl` with these commands only reads and writes the flags of
    // `fd`.
    unsafe {
        let flags = libc::fcntl(fd, get);
        if flags < 0 || libc::fcntl(fd, set, flags | flag) < 0 {
            return Err(last_error());
        }
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    set_flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
}

fn set_close_on_exec(fd: RawFd) -> io::Result<()> {
    set_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)
}

/// Keeps whatever the process that started a holder left open (from sshd,
/// say) out of the shell the holder starts.
fn mark_inherited_descriptors_close_on_exec() {
    let Ok(entries) = std::fs::read_dir("/dev/fd") else {
        return;
    };
    let descriptors: Vec<RawFd> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    for fd in descriptors {
        // The directory's own descriptor is closed by now.
        set_close_on_exec(fd).ok();
    }
}

static SIGNAL_PIPE_WRITER: AtomicI32 = AtomicI32::new(-1);

#[cfg(any(target_os = "linux", target_os = "android"))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's `errno`.
    unsafe { libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn errno_location() -> *mut libc::c_int {
    // SAFETY: returns the calling thread's `errno`.
    unsafe { libc::__error() }
}

extern "C" fn forward_signal(signal: libc::c_int) {
    // A write can change `errno` under the code the signal interrupted.
    let errno = errno_location();
    // SAFETY: `errno` is this thread's; `write` is async-signal-safe and the
    // byte outlives the call.
    unsafe {
        let saved = *errno;
        let fd = SIGNAL_PIPE_WRITER.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = signal as u8;
            libc::write(fd, (&raw const byte).cast(), 1);
        }
        *errno = saved;
    }
}

/// Turns signals into bytes on a pipe, so that the loops wait for them the
/// same way they wait for everything else.
struct SignalPipe {
    reader: OwnedFd,
}

impl SignalPipe {
    fn install(signals: &[libc::c_int]) -> io::Result<Self> {
        let (reader, writer) = io::pipe()?;
        let reader = OwnedFd::from(reader);
        let writer = OwnedFd::from(writer);
        set_nonblocking(reader.as_raw_fd())?;
        set_nonblocking(writer.as_raw_fd())?;
        // The handler writes to it for as long as the process lives.
        let previous = SIGNAL_PIPE_WRITER.swap(writer.into_raw_fd(), Ordering::SeqCst);
        if previous >= 0 {
            // SAFETY: `previous` was the writer of an earlier pipe, which
            // nothing else owns.
            drop(unsafe { OwnedFd::from_raw_fd(previous) });
        }
        for &signal in signals {
            // SAFETY: `action` is fully initialized before `sigaction` reads
            // it, and `forward_signal` only does async-signal-safe work.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = forward_signal as extern "C" fn(libc::c_int) as usize;
                action.sa_flags = libc::SA_RESTART
                    | if signal == libc::SIGCHLD {
                        libc::SA_NOCLDSTOP
                    } else {
                        0
                    };
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                    return Err(last_error());
                }
            }
        }
        Ok(Self { reader })
    }

    fn fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }

    fn take(&self) -> Vec<libc::c_int> {
        let mut signals = Vec::new();
        let mut buffer = [0u8; 64];
        loop {
            // SAFETY: `buffer` outlives the call and is as long as we say.
            let read = unsafe { libc::read(self.fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if read <= 0 {
                if read < 0 && last_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            signals.extend(
                buffer[..read as usize]
                    .iter()
                    .map(|&byte| libc::c_int::from(byte)),
            );
        }
        signals
    }
}

#[derive(Clone, Copy)]
struct Interest {
    fd: RawFd,
    read: bool,
    write: bool,
}

#[derive(Clone, Copy, Default)]
struct Readiness {
    readable: bool,
    writable: bool,
}

/// Waits until one of `interests` can be read or written (as far as each
/// asks for), or until `timeout` passes. A signal ends the wait early with
/// nothing ready.
#[cfg(not(target_os = "macos"))]
fn wait(interests: &[Interest], timeout: Option<Duration>) -> io::Result<Vec<Readiness>> {
    let mut descriptors: Vec<libc::pollfd> = interests
        .iter()
        .map(|interest| libc::pollfd {
            fd: interest.fd,
            events: if interest.read { libc::POLLIN } else { 0 }
                | if interest.write { libc::POLLOUT } else { 0 },
            revents: 0,
        })
        .collect();
    let timeout = timeout.map_or(-1, |timeout| {
        // Rounded up, so that a deadline is not woken up for a little early
        // over and over.
        timeout.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as libc::c_int
    });
    // SAFETY: `descriptors` outlives the call and holds as many entries as we
    // say.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            timeout,
        )
    };
    if result < 0 {
        let error = last_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(vec![Readiness::default(); interests.len()]);
        }
        return Err(error);
    }
    Ok(descriptors
        .iter()
        .zip(interests)
        .map(|(descriptor, interest)| {
            let events = descriptor.revents;
            let failed = events & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            Readiness {
                readable: interest.read && (failed || events & libc::POLLIN != 0),
                writable: interest.write && (failed || events & libc::POLLOUT != 0),
            }
        })
        .collect())
}

/// macOS's `poll` does not work on terminal devices, which an attach's stdin
/// is.
#[cfg(target_os = "macos")]
fn wait(interests: &[Interest], timeout: Option<Duration>) -> io::Result<Vec<Readiness>> {
    // SAFETY: the sets are initialized by `FD_ZERO` before use, every
    // descriptor is checked against `FD_SETSIZE`, and the sets and `timeval`
    // outlive the `select` call.
    unsafe {
        let mut readable: libc::fd_set = std::mem::zeroed();
        let mut writable: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut readable);
        libc::FD_ZERO(&mut writable);
        let mut highest = -1;
        for interest in interests {
            if interest.fd < 0 || interest.fd as usize >= libc::FD_SETSIZE as usize {
                return Err(io::Error::other("a descriptor is too large for select"));
            }
            if interest.read {
                libc::FD_SET(interest.fd, &mut readable);
            }
            if interest.write {
                libc::FD_SET(interest.fd, &mut writable);
            }
            highest = highest.max(interest.fd);
        }
        let mut timeval = timeout.map(|timeout| libc::timeval {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        });
        let result = libc::select(
            highest + 1,
            &mut readable,
            &mut writable,
            std::ptr::null_mut(),
            timeval.as_mut().map_or(std::ptr::null_mut(), |timeval| {
                timeval as *mut libc::timeval
            }),
        );
        if result < 0 {
            let error = last_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(vec![Readiness::default(); interests.len()]);
            }
            return Err(error);
        }
        Ok(interests
            .iter()
            .map(|interest| Readiness {
                readable: interest.read && libc::FD_ISSET(interest.fd, &readable),
                writable: interest.write && libc::FD_ISSET(interest.fd, &writable),
            })
            .collect())
    }
}

/// A connection to an attach (or to a process on its way to becoming one).
struct Connection {
    stream: UnixStream,
    decoder: Decoder,
    outgoing: Vec<u8>,
    written: usize,
    /// When the other side last took some of what it was sent, or when it
    /// was last sent something while it had nothing left to take.
    last_progress: Instant,
}

impl Connection {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            decoder: Decoder::default(),
            outgoing: Vec::new(),
            written: 0,
            last_progress: Instant::now(),
        })
    }

    fn send(&mut self, message: &ToAttach) {
        if self.unsent() == 0 {
            self.last_progress = Instant::now();
        }
        message.encode(&mut self.outgoing);
    }

    fn unsent(&self) -> usize {
        self.outgoing.len() - self.written
    }

    /// When the other side counts as gone for having taken nothing of what
    /// it has been sent. `None` while it has nothing to take.
    fn stalled_at(&self, timeout: Duration) -> Option<Instant> {
        (self.unsent() > 0).then(|| self.last_progress + timeout)
    }

    /// Writes what the socket takes without waiting.
    fn flush(&mut self) -> io::Result<()> {
        while self.written < self.outgoing.len() {
            match self.stream.write(&self.outgoing[self.written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(written) => {
                    self.written += written;
                    self.last_progress = Instant::now();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        if self.written == self.outgoing.len() {
            self.outgoing.clear();
            self.written = 0;
        } else if self.written >= READ_CHUNK * 16 {
            self.outgoing.drain(..self.written);
            self.written = 0;
        }
        Ok(())
    }

    /// Reads what has arrived. `Ok(false)` once the other side has closed.
    fn receive(&mut self) -> io::Result<bool> {
        let mut buffer = [0u8; 16 * 1024];
        match self.stream.read(&mut buffer) {
            Ok(0) => Ok(false),
            Ok(read) => {
                self.decoder.extend(&buffer[..read]);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

struct HolderOptions {
    id: String,
    detached_timeout: Option<Duration>,
    replay_bytes: usize,
    program: Vec<OsString>,
}

enum Shell {
    NotStarted,
    Running(RunningShell),
    Exited(ShellStatus),
}

struct RunningShell {
    pid: libc::pid_t,
    master: File,
    /// Held open so that the master never reads as hung up, whatever the
    /// shell does with its own descriptors; the shell's end is told by
    /// SIGCHLD instead.
    _slave: OwnedFd,
    unwritten_input: Vec<u8>,
}

struct Pending {
    connection: Connection,
    accepted_at: Instant,
    introduced: bool,
}

struct Retiring {
    connection: Connection,
    deadline: Instant,
}

struct Termination {
    kill_at: Instant,
    give_up_at: Instant,
    killed: bool,
    foreground_group: libc::pid_t,
}

struct Holder {
    options: HolderOptions,
    paths: SessionPaths,
    listener: UnixListener,
    socket_identity: Option<(u64, u64)>,
    lock: File,
    signals: SignalPipe,
    shell: Shell,
    history: OutputHistory,
    size: WindowSize,
    attached: Option<Connection>,
    /// The attach has been told everything the holder is here for (the
    /// shell's exit, or why the shell could not start).
    exit_once_flushed: bool,
    pending: Vec<Pending>,
    retiring: Vec<Retiring>,
    detached_since: Option<Instant>,
    redraw_restore_at: Option<Instant>,
    termination: Option<Termination>,
    stalled_attach_timeout: Duration,
    read_buffer: Vec<u8>,
}

fn run_holder(options: HolderOptions) -> i32 {
    mark_inherited_descriptors_close_on_exec();
    let id = options.id.clone();
    match Holder::start(options) {
        Ok(Some(mut holder)) => {
            holder.run();
            holder.clean_up();
            log::info!("terminal session {id} ended");
            0
        }
        Ok(None) => {
            log::info!("another holder already keeps terminal session {id}");
            0
        }
        Err(error) => {
            log::error!("could not keep terminal session {id}: {error:#}");
            EXIT_FAILURE
        }
    }
}

/// Locks the session for this holder. `None` when another holder turns out
/// to serve it.
fn acquire_holder_lock(paths: &SessionPaths) -> Result<Option<File>> {
    let started = Instant::now();
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&paths.lock)
            .with_context(|| format!("opening {}", paths.lock.display()))?;
        match file.try_lock() {
            Ok(()) => {
                // A holder that exits unlinks the lock file while holding it;
                // a lock on a file that is no longer there locks nothing.
                let locked = file
                    .metadata()
                    .ok()
                    .map(|metadata| (metadata.dev(), metadata.ino()));
                if locked.is_some() && locked == file_identity(&paths.lock) {
                    return Ok(Some(file));
                }
                continue;
            }
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking {}", paths.lock.display()));
            }
        }
        if UnixStream::connect(&paths.socket).is_ok() {
            return Ok(None);
        }
        if started.elapsed() > HOLDER_LOCK_TIMEOUT {
            bail!("another holder kept the session locked");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl Holder {
    fn start(options: HolderOptions) -> Result<Option<Self>> {
        let paths = SessionPaths::resolve(&sessions_directory(), &options.id, true)?;
        let Some(lock) = acquire_holder_lock(&paths)? else {
            return Ok(None);
        };
        // Whatever socket is there belongs to a holder that died without
        // cleaning up: a live one would still hold the lock.
        crate::remove_file_if_present(&paths.socket)
            .with_context(|| format!("removing a stale {}", paths.socket.display()))?;
        let listener = UnixListener::bind(&paths.socket)
            .with_context(|| format!("binding {}", paths.socket.display()))?;
        std::fs::set_permissions(&paths.socket, std::fs::Permissions::from_mode(0o600))
            .context("restricting the socket to this user")?;
        listener.set_nonblocking(true)?;
        let socket_identity = file_identity(&paths.socket);
        let signals =
            SignalPipe::install(&[libc::SIGCHLD, libc::SIGTERM, libc::SIGINT, libc::SIGHUP])
                .context("installing signal handlers")?;
        log::info!(
            "keeping terminal session {} (detached timeout {:?}, replay {} bytes)",
            options.id,
            options.detached_timeout,
            options.replay_bytes
        );
        Ok(Some(Self {
            history: OutputHistory::new(options.replay_bytes),
            options,
            paths,
            listener,
            socket_identity,
            lock,
            signals,
            shell: Shell::NotStarted,
            size: WindowSize::FALLBACK,
            attached: None,
            exit_once_flushed: false,
            pending: Vec::new(),
            retiring: Vec::new(),
            detached_since: Some(Instant::now()),
            redraw_restore_at: None,
            termination: None,
            stalled_attach_timeout: stalled_attach_timeout(),
            read_buffer: vec![0; READ_CHUNK],
        }))
    }

    fn run(&mut self) {
        loop {
            self.handle_deadlines(Instant::now());
            if self.finished() {
                return;
            }

            #[derive(Clone, Copy)]
            enum Source {
                Listener,
                Signals,
                Shell,
                Attached,
                Pending(usize),
                Retiring(usize),
            }
            let mut sources = vec![Source::Listener, Source::Signals];
            let mut interests = vec![
                Interest {
                    fd: self.listener.as_raw_fd(),
                    read: true,
                    write: false,
                },
                Interest {
                    fd: self.signals.fd(),
                    read: true,
                    write: false,
                },
            ];
            if let Shell::Running(shell) = &self.shell {
                let read = !self.output_backlogged();
                let write = !shell.unwritten_input.is_empty();
                if read || write {
                    sources.push(Source::Shell);
                    interests.push(Interest {
                        fd: shell.master.as_raw_fd(),
                        read,
                        write,
                    });
                }
            }
            if let Some(attached) = &self.attached {
                sources.push(Source::Attached);
                interests.push(Interest {
                    fd: attached.stream.as_raw_fd(),
                    read: true,
                    write: attached.unsent() > 0,
                });
            }
            for (index, pending) in self.pending.iter().enumerate() {
                sources.push(Source::Pending(index));
                interests.push(Interest {
                    fd: pending.connection.stream.as_raw_fd(),
                    read: true,
                    write: pending.connection.unsent() > 0,
                });
            }
            for (index, retiring) in self.retiring.iter().enumerate() {
                sources.push(Source::Retiring(index));
                // Not read: what a retired attach still sends is of no use,
                // and leaving it unread would wake every wait.
                interests.push(Interest {
                    fd: retiring.connection.stream.as_raw_fd(),
                    read: false,
                    write: true,
                });
            }

            let timeout = self
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let readiness = match wait(&interests, timeout) {
                Ok(readiness) => readiness,
                Err(error) => {
                    log::error!("waiting for events failed: {error}");
                    return;
                }
            };

            let mut ready_pending = Vec::new();
            let mut ready_retiring = Vec::new();
            let mut listener_ready = false;
            for (source, readiness) in sources.into_iter().zip(readiness) {
                match source {
                    Source::Signals if readiness.readable => self.handle_signals(),
                    Source::Shell => {
                        if readiness.readable {
                            self.read_shell_output(false);
                        }
                        if readiness.writable {
                            self.write_shell_input();
                        }
                    }
                    Source::Attached => {
                        if readiness.readable {
                            self.receive_from_attached();
                        }
                        if readiness.writable {
                            self.flush_attached();
                        }
                    }
                    Source::Listener if readiness.readable => listener_ready = true,
                    Source::Pending(index) if readiness.readable || readiness.writable => {
                        ready_pending.push(index)
                    }
                    Source::Retiring(index) if readiness.readable || readiness.writable => {
                        ready_retiring.push(index)
                    }
                    _ => {}
                }
            }
            // Highest first, so that removing one does not move the others.
            for index in ready_retiring.into_iter().rev() {
                self.flush_retiring(index);
            }
            for index in ready_pending.into_iter().rev() {
                self.advance_pending(index);
            }
            if listener_ready {
                self.accept_connections();
            }
        }
    }

    fn finished(&self) -> bool {
        if let Some(termination) = &self.termination {
            // The connections let go are given their moment (bounded by
            // their own deadline) to learn that the session ended.
            return (!matches!(self.shell, Shell::Running(_)) && self.retiring.is_empty())
                || Instant::now() >= termination.give_up_at;
        }
        self.exit_once_flushed
            && self
                .attached
                .as_ref()
                .is_some_and(|attached| attached.unsent() == 0)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let mut deadlines = Vec::new();
        if let Some(termination) = &self.termination {
            deadlines.push(if termination.killed {
                termination.give_up_at
            } else {
                termination.kill_at
            });
        }
        deadlines.extend(self.detached_deadline());
        deadlines.extend(
            self.attached
                .as_ref()
                .and_then(|attached| attached.stalled_at(self.stalled_attach_timeout)),
        );
        deadlines.extend(self.redraw_restore_at);
        deadlines.extend(
            self.pending
                .iter()
                .map(|pending| pending.accepted_at + HANDSHAKE_TIMEOUT),
        );
        deadlines.extend(self.retiring.iter().map(|retiring| retiring.deadline));
        deadlines.into_iter().min()
    }

    /// When the session ends for having nothing attached. `None` while
    /// something is, and for a timeout too long to be reached.
    fn detached_deadline(&self) -> Option<Instant> {
        if self.termination.is_some() || self.attached.is_some() {
            return None;
        }
        let shell_started = !matches!(self.shell, Shell::NotStarted);
        self.detached_since?.checked_add(holder_detached_timeout(
            self.options.detached_timeout,
            shell_started,
            unattached_holder_timeout(),
        )?)
    }

    fn handle_deadlines(&mut self, now: Instant) {
        if self
            .attached
            .as_ref()
            .and_then(|attached| attached.stalled_at(self.stalled_attach_timeout))
            .is_some_and(|stalled_at| now >= stalled_at)
        {
            self.detach("the attach stopped taking output");
        }
        if self
            .detached_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            self.begin_termination("nothing was attached for the detached timeout");
        }
        if let Some(termination) = &mut self.termination
            && !termination.killed
            && now >= termination.kill_at
        {
            termination.killed = true;
            if let Shell::Running(shell) = &self.shell {
                log::info!("the shell outlived its hangup; killing it");
                signal_shell(shell.pid, termination.foreground_group, libc::SIGKILL);
            }
        }
        if let Some(restore_at) = self.redraw_restore_at
            && now >= restore_at
        {
            self.redraw_restore_at = None;
            self.apply_size(self.size);
        }
        self.pending
            .retain(|pending| now < pending.accepted_at + HANDSHAKE_TIMEOUT);
        self.retiring.retain(|retiring| now < retiring.deadline);
    }

    fn handle_signals(&mut self) {
        for signal in self.signals.take() {
            match signal {
                libc::SIGCHLD => self.reap_children(),
                libc::SIGTERM | libc::SIGINT => self.begin_termination("asked to end by a signal"),
                // A holder has no terminal whose hangup could concern it.
                _ => {}
            }
        }
    }

    fn reap_children(&mut self) {
        loop {
            let mut status = 0;
            // SAFETY: `status` outlives the call, which only writes to it.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                return;
            }
            let Shell::Running(shell) = &self.shell else {
                continue;
            };
            if shell.pid != pid {
                continue;
            }
            let status = if libc::WIFEXITED(status) {
                ShellStatus::Exited(libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                ShellStatus::Signaled(libc::WTERMSIG(status))
            } else {
                continue;
            };
            self.shell_exited(status);
        }
    }

    fn shell_exited(&mut self, status: ShellStatus) {
        self.read_shell_output(true);
        self.shell = Shell::Exited(status);
        self.redraw_restore_at = None;
        log::info!("the shell ended ({status:?})");
        if self.termination.is_some() {
            return;
        }
        if let Some(attached) = &mut self.attached {
            attached.send(&ToAttach::ShellExited {
                status,
                while_detached: false,
            });
            self.exit_once_flushed = true;
        }
        self.flush_attached();
    }

    /// Reads the shell's output once, or until there is none left.
    fn read_shell_output(&mut self, until_empty: bool) {
        let Shell::Running(shell) = &mut self.shell else {
            return;
        };
        loop {
            match shell.master.read(&mut self.read_buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let output = &self.read_buffer[..read];
                    self.history.push(output);
                    if let Some(attached) = &mut self.attached {
                        attached.send(&ToAttach::Output(output.to_vec()));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                // The slave side has been closed for good.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => {
                    log::warn!("reading the shell's output failed: {error}");
                    break;
                }
            }
            if !until_empty {
                break;
            }
        }
        self.flush_attached();
    }

    fn output_backlogged(&self) -> bool {
        self.attached
            .as_ref()
            .is_some_and(|attached| attached.unsent() >= OUTPUT_BACKLOG_LIMIT)
    }

    fn write_shell_input(&mut self) {
        let Shell::Running(shell) = &mut self.shell else {
            return;
        };
        while !shell.unwritten_input.is_empty() {
            match shell.master.write(&shell.unwritten_input) {
                Ok(written) => {
                    shell.unwritten_input.drain(..written);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log::warn!("writing to the shell failed: {error}");
                    shell.unwritten_input.clear();
                    break;
                }
            }
        }
    }

    fn receive_from_attached(&mut self) {
        let Some(attached) = &mut self.attached else {
            return;
        };
        match attached.receive() {
            Ok(true) => {}
            Ok(false) => return self.detach("the attach went away"),
            Err(error) => return self.detach(&format!("reading from the attach failed: {error}")),
        }
        let mut messages = Vec::new();
        loop {
            match attached.decoder.take_to_holder() {
                Ok(Some(message)) => messages.push(message),
                Ok(None) => break,
                Err(error) => return self.detach(&format!("the attach sent garbage: {error:#}")),
            }
        }
        for message in messages {
            match message {
                ToHolder::Input(input) => {
                    if let Shell::Running(shell) = &mut self.shell {
                        if shell.unwritten_input.len() + input.len() > MAX_UNWRITTEN_INPUT {
                            log::warn!("the shell is not reading its input; dropping some");
                            continue;
                        }
                        shell.unwritten_input.extend_from_slice(&input);
                    }
                }
                ToHolder::Resize(size) => {
                    self.size = usable_size(size);
                    if self.redraw_restore_at.is_none() {
                        self.apply_size(self.size);
                    }
                }
                ToHolder::Hello { .. } => {
                    return self.detach("the attach introduced itself twice");
                }
            }
        }
        self.write_shell_input();
    }

    fn flush_attached(&mut self) {
        if let Some(attached) = &mut self.attached
            && let Err(error) = attached.flush()
        {
            self.detach(&format!("writing to the attach failed: {error}"));
        }
    }

    fn detach(&mut self, reason: &str) {
        if self.attached.take().is_some() {
            log::info!("detached: {reason}");
            self.detached_since = Some(Instant::now());
            // Whatever it was to be told did not reach it for sure; the next
            // attach is told instead.
            self.exit_once_flushed = false;
        }
    }

    fn accept_connections(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if !is_this_user(&stream) {
                        continue;
                    }
                    match Connection::new(stream) {
                        Ok(connection) => self.pending.push(Pending {
                            connection,
                            accepted_at: Instant::now(),
                            introduced: false,
                        }),
                        Err(error) => log::warn!("could not take a connection: {error}"),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    // Running out of descriptors, or a connection that went
                    // away before it was taken: the next wakeup tries again.
                    log::warn!("accepting a connection failed: {error}");
                    return;
                }
            }
        }
    }

    fn advance_pending(&mut self, index: usize) {
        let Some(pending) = self.pending.get_mut(index) else {
            return;
        };
        match pending.connection.receive() {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                self.pending.remove(index);
                return;
            }
        }
        if !pending.introduced {
            let preamble = match pending.connection.decoder.take_preamble() {
                Ok(Some(preamble)) => preamble,
                Ok(None) => return,
                Err(error) => {
                    log::warn!("turned away a connection: {error:#}");
                    self.pending.remove(index);
                    return;
                }
            };
            pending.introduced = true;
            pending
                .connection
                .outgoing
                .extend_from_slice(&encode_preamble(Role::Holder));
            match preamble.role {
                Role::Kill => {
                    let pending = self.pending.remove(index);
                    self.retire(pending.connection);
                    self.begin_termination("asked to end by `terminal-session kill`");
                    return;
                }
                Role::Attach if preamble.version != PROTOCOL_VERSION => {
                    log::warn!(
                        "turned away an attach that speaks protocol {} (this holder speaks {PROTOCOL_VERSION})",
                        preamble.version
                    );
                    let pending = self.pending.remove(index);
                    self.retire(pending.connection);
                    return;
                }
                Role::Attach => {}
                Role::Holder => {
                    self.pending.remove(index);
                    return;
                }
            }
        }
        match pending.connection.decoder.take_to_holder() {
            Ok(None) => {
                if let Err(error) = pending.connection.flush() {
                    log::warn!("lost a connection during its introduction: {error}");
                    self.pending.remove(index);
                }
            }
            Ok(Some(ToHolder::Hello { size, id })) => {
                let pending = self.pending.remove(index);
                self.accept_attach(pending.connection, size, id);
            }
            Ok(Some(_)) | Err(_) => {
                log::warn!("turned away a connection that did not introduce itself");
                self.pending.remove(index);
            }
        }
    }

    fn accept_attach(&mut self, mut connection: Connection, size: WindowSize, id: String) {
        if id != self.options.id {
            connection.send(&ToAttach::Error(format!(
                "this terminal session holder keeps session {}, not {id}",
                self.options.id
            )));
            return self.retire(connection);
        }
        if self.termination.is_some() {
            connection.send(&ToAttach::Error(
                "the terminal session is ending".to_string(),
            ));
            return self.retire(connection);
        }
        if let Some(mut previous) = self.attached.take() {
            log::info!("a new attach takes over from the previous one");
            previous.send(&ToAttach::TakenOver);
            self.retire(previous);
        } else {
            log::info!("attached");
        }
        self.detached_since = None;
        self.exit_once_flushed = false;
        self.size = usable_size(size);

        match self.shell {
            Shell::NotStarted => {
                if let Err(error) = self.start_shell() {
                    log::error!("could not start the shell: {error:#}");
                    // The attach has already begun the replay on its stdout.
                    connection.send(&ToAttach::ReplayEnd);
                    connection.send(&ToAttach::Error(format!(
                        "could not start the shell: {error:#}"
                    )));
                    self.exit_once_flushed = true;
                }
            }
            Shell::Running(_) => {
                if self.redraw_restore_at.is_none() {
                    self.apply_size(self.size);
                }
            }
            Shell::Exited(_) => {}
        }

        if !self.exit_once_flushed {
            let replay = self.history.replay();
            connection.send(&ToAttach::Replay(replay.bytes));
            connection.send(&ToAttach::ReplayEnd);
            match &self.shell {
                Shell::Exited(status) => {
                    connection.send(&ToAttach::ShellExited {
                        status: *status,
                        while_detached: true,
                    });
                    self.exit_once_flushed = true;
                }
                Shell::Running(_) if replay.needs_redraw && self.size.rows > 1 => {
                    self.apply_size(WindowSize {
                        rows: self.size.rows - 1,
                        cols: self.size.cols,
                    });
                    self.redraw_restore_at = Some(Instant::now() + REDRAW_DELAY);
                }
                _ => {}
            }
        }
        self.attached = Some(connection);
        self.flush_attached();
    }

    /// Gives a connection that is let go a moment to take in what it was
    /// last sent.
    fn retire(&mut self, mut connection: Connection) {
        if connection.flush().is_ok() && connection.unsent() > 0 {
            self.retiring.push(Retiring {
                connection,
                deadline: Instant::now() + RETIRING_FLUSH_TIMEOUT,
            });
        }
    }

    fn flush_retiring(&mut self, index: usize) {
        let Some(retiring) = self.retiring.get_mut(index) else {
            return;
        };
        if retiring.connection.flush().is_err() || retiring.connection.unsent() == 0 {
            self.retiring.remove(index);
        }
    }

    fn apply_size(&self, size: WindowSize) {
        if let Shell::Running(shell) = &self.shell
            && let Err(error) = set_window_size(shell.master.as_raw_fd(), size)
        {
            log::warn!("resizing the terminal failed: {error}");
        }
    }

    fn start_shell(&mut self) -> Result<()> {
        let program = self.options.program.first().context("no program to run")?;
        let path = find_program(program)?;
        let path = CString::new(path.as_os_str().as_bytes()).context("a program path with NUL")?;
        let arguments = self
            .options
            .program
            .iter()
            .map(|argument| CString::new(argument.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .context("an argument with NUL")?;
        let mut argv: Vec<*const libc::c_char> =
            arguments.iter().map(|argument| argument.as_ptr()).collect();
        argv.push(std::ptr::null());
        let environment = shell_environment(std::env::vars_os())?;
        let mut envp: Vec<*const libc::c_char> = environment
            .iter()
            .map(|variable| variable.as_ptr())
            .collect();
        envp.push(std::ptr::null());

        let (master, slave) = open_pty(self.size).context("opening a pseudo-terminal")?;
        // SAFETY: the child only makes async-signal-safe calls before `exec`,
        // on data prepared before the fork.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(last_error()).context("forking the shell");
        }
        if pid == 0 {
            // SAFETY: this is the child of the fork above.
            unsafe { exec_shell(slave.as_raw_fd(), &path, &argv, &envp) }
        }
        log::info!("started the shell (pid {pid})");
        self.shell = Shell::Running(RunningShell {
            pid,
            master,
            _slave: slave,
            unwritten_input: Vec::new(),
        });
        // The shell has its working directory; the holder would only keep
        // that directory from being unmounted.
        std::env::set_current_dir("/").log_err();
        Ok(())
    }

    /// Hangs the shell up, and kills it after [`TERMINATION_GRACE`].
    fn begin_termination(&mut self, reason: &str) {
        if self.termination.is_some() {
            return;
        }
        log::info!("ending the terminal session: {reason}");
        // An attach that comes now starts a new holder instead of reaching
        // one that is going away.
        self.remove_socket();
        // Told, so that it ends as a failure rather than as a lost
        // connection, which its terminal would try to resume.
        if let Some(mut attached) = self.attached.take() {
            attached.send(&ToAttach::Error(SESSION_ENDED_REASON.to_string()));
            self.retire(attached);
        }
        self.pending.clear();
        self.redraw_restore_at = None;
        let foreground_group = match &self.shell {
            Shell::Running(shell) => {
                // SAFETY: `tcgetpgrp` only reads the terminal's foreground
                // process group.
                let group = unsafe { libc::tcgetpgrp(shell.master.as_raw_fd()) };
                signal_shell(shell.pid, group, libc::SIGHUP);
                group
            }
            _ => 0,
        };
        let now = Instant::now();
        self.termination = Some(Termination {
            kill_at: now + TERMINATION_GRACE,
            give_up_at: now + TERMINATION_GRACE + TERMINATION_GIVE_UP,
            killed: false,
            foreground_group,
        });
    }

    fn remove_socket(&mut self) {
        if let Some(identity) = self.socket_identity.take() {
            remove_if_identical(&self.paths.socket, identity);
        }
    }

    fn clean_up(&mut self) {
        self.remove_socket();
        // Unlinked while still locked (see `acquire_holder_lock`).
        if let Ok(metadata) = self.lock.metadata() {
            remove_if_identical(&self.paths.lock, (metadata.dev(), metadata.ino()));
        }
    }
}

fn usable_size(size: WindowSize) -> WindowSize {
    if size.rows == 0 || size.cols == 0 {
        WindowSize::FALLBACK
    } else {
        size
    }
}

/// Sends `signal` to the shell's process group and to the job in the
/// foreground, then continues both so that stopped jobs see it too.
fn signal_shell(shell: libc::pid_t, foreground_group: libc::pid_t, signal: libc::c_int) {
    let mut groups = vec![shell];
    if foreground_group > 0 && foreground_group != shell {
        groups.push(foreground_group);
    }
    for group in groups {
        // SAFETY: `killpg` only sends signals.
        unsafe {
            libc::killpg(group, signal);
            if signal != libc::SIGKILL {
                libc::killpg(group, libc::SIGCONT);
            }
        }
    }
}

/// The holder's environment for its shell, without the display language
/// the client gave this process, which is not the user's to see.
fn shell_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Result<Vec<CString>> {
    variables
        .into_iter()
        .filter(|(name, _)| name != remote::DISPLAY_LANGUAGE_ENV_VAR)
        .map(|(name, value)| {
            let mut variable = name.into_vec();
            variable.push(b'=');
            variable.extend_from_slice(value.as_bytes());
            CString::new(variable).context("an environment variable with NUL")
        })
        .collect()
}

fn find_program(program: &OsStr) -> Result<PathBuf> {
    if program.as_bytes().contains(&b'/') {
        return Ok(PathBuf::from(program));
    }
    let search_path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    for directory in std::env::split_paths(&search_path) {
        let candidate = directory.join(program);
        if std::fs::metadata(&candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        {
            return Ok(candidate);
        }
    }
    bail!("`{}` is not on PATH", program.to_string_lossy())
}

fn set_window_size(fd: RawFd, size: WindowSize) -> io::Result<()> {
    let window_size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `window_size` outlives the call, which only reads it.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &window_size) } != 0 {
        return Err(last_error());
    }
    Ok(())
}

fn window_size_of(fd: RawFd) -> Option<WindowSize> {
    // SAFETY: an all-zero `winsize` is valid, and `ioctl` only writes to it.
    let mut window_size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut window_size) } != 0 {
        return None;
    }
    (window_size.ws_row > 0 && window_size.ws_col > 0).then_some(WindowSize {
        rows: window_size.ws_row,
        cols: window_size.ws_col,
    })
}

fn open_pty(size: WindowSize) -> io::Result<(File, OwnedFd)> {
    // SAFETY: every descriptor is owned as soon as it is created, and the
    // buffers passed in outlive the calls.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(last_error());
        }
        let master = OwnedFd::from_raw_fd(master);
        set_close_on_exec(master.as_raw_fd())?;
        if libc::grantpt(master.as_raw_fd()) != 0 || libc::unlockpt(master.as_raw_fd()) != 0 {
            return Err(last_error());
        }
        let name = pty_name(master.as_raw_fd())?;
        let slave = libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        );
        if slave < 0 {
            return Err(last_error());
        }
        let slave = OwnedFd::from_raw_fd(slave);
        #[cfg(target_os = "linux")]
        {
            // As tmux does: without it, erasing a multibyte character in a
            // line being typed (`cat`, `read`) erases only its last byte.
            let mut attributes: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave.as_raw_fd(), &mut attributes) == 0 {
                attributes.c_iflag |= libc::IUTF8;
                libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &attributes);
            }
        }
        set_window_size(master.as_raw_fd(), size)?;
        set_nonblocking(master.as_raw_fd())?;
        Ok((File::from(master), slave))
    }
}

#[cfg(target_os = "linux")]
fn pty_name(master: RawFd) -> io::Result<CString> {
    let mut buffer = [0 as libc::c_char; 128];
    // SAFETY: `buffer` outlives the call and is as long as we say.
    let result = unsafe { libc::ptsname_r(master, buffer.as_mut_ptr(), buffer.len()) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    // SAFETY: `ptsname_r` wrote a NUL-terminated string into `buffer`.
    Ok(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_owned())
}

#[cfg(not(target_os = "linux"))]
fn pty_name(master: RawFd) -> io::Result<CString> {
    // SAFETY: `ptsname` returns a static buffer, copied before anything else
    // could overwrite it (a holder has a single thread).
    unsafe {
        let name = libc::ptsname(master);
        if name.is_null() {
            return Err(last_error());
        }
        Ok(CStr::from_ptr(name).to_owned())
    }
}

/// The child's side of starting the shell: a session of its own, the
/// pseudo-terminal as its controlling terminal and stdio, and the signal
/// dispositions a fresh process has.
///
/// # Safety
///
/// Must be called in the child right after `fork`.
unsafe fn exec_shell(
    slave: RawFd,
    path: &CStr,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> ! {
    // SAFETY: all of these are async-signal-safe, and their arguments were
    // prepared before the fork.
    unsafe {
        libc::setsid();
        libc::ioctl(slave, libc::TIOCSCTTY as _, 0 as libc::c_int);
        for target in 0..3 {
            libc::dup2(slave, target);
        }
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut signals);
        libc::sigprocmask(libc::SIG_SETMASK, &signals, std::ptr::null_mut());
        for signal in 1..32 {
            libc::signal(signal, libc::SIG_DFL);
        }
        libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
        let message = b"zed: could not start the shell\r\n";
        libc::write(2, message.as_ptr().cast(), message.len());
        libc::_exit(127)
    }
}

struct AttachOptions {
    id: String,
    nonce: String,
    resume: bool,
    detached_timeout_secs: u64,
    replay_bytes: usize,
    program: Vec<OsString>,
}

enum Outcome {
    Exit(i32),
    /// Fails with this reason, told on stderr once the terminal is back to
    /// how it was.
    Fail(String),
    /// This end of the session went away (see [`EXIT_DETACHED`]) for this
    /// reason, which goes to the log only: the shell lives on.
    Detached(String),
}

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;

fn attach(options: &AttachOptions) -> i32 {
    let outcome = match SignalPipe::install(&[
        libc::SIGWINCH,
        libc::SIGHUP,
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGQUIT,
    ]) {
        Ok(signals) => attach_with(options, &signals),
        Err(error) => Outcome::Fail(format!("could not install signal handlers: {error}")),
    };
    match outcome {
        Outcome::Exit(code) => code,
        Outcome::Detached(reason) => {
            log::info!("detached from terminal session {}: {reason}", options.id);
            EXIT_DETACHED
        }
        Outcome::Fail(reason) => {
            let (level, message) = attach_failure_log(&options.id, &reason);
            log::log!(level, "{message}");
            print_error(&reason);
            EXIT_FAILURE
        }
    }
}

fn attach_with(options: &AttachOptions, signals: &SignalPipe) -> Outcome {
    let paths = match SessionPaths::resolve(&sessions_directory(), &options.id, true) {
        Ok(paths) => paths,
        Err(error) => return Outcome::Fail(format!("{error:#}")),
    };
    let (stream, started) = match connect_or_start(&paths, options) {
        Ok(connected) => connected,
        Err(reason) => return Outcome::Fail(reason),
    };
    let size = terminal_size();
    let stream = match introduce(stream, &options.id, size) {
        Ok(stream) => stream,
        Err(reason) => return Outcome::Fail(reason),
    };
    let raw_mode = RawMode::enable();
    let outcome = relay(stream, started && options.resume, &options.nonce, signals);
    drop(raw_mode);
    outcome
}

fn connect_or_start(
    paths: &SessionPaths,
    options: &AttachOptions,
) -> Result<(UnixStream, bool), String> {
    match UnixStream::connect(&paths.socket) {
        Ok(stream) => return Ok((stream, false)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => {
            return Err(format!(
                "could not reach the terminal session holder: {error}"
            ));
        }
    }
    let mut holder = spawn_holder(options)?;
    let started = Instant::now();
    let mut holder_exit = None;
    loop {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(stream) = UnixStream::connect(&paths.socket) {
            return Ok((stream, true));
        }
        if holder_exit.is_none()
            && let Ok(Some(status)) = holder.try_wait()
        {
            // Exiting successfully means another holder won the race to
            // keep this session; it is about to accept connections.
            if !status.success() {
                return Err(format!(
                    "the terminal session holder could not start ({status}); see {}",
                    log_path().display()
                ));
            }
            holder_exit = Some(status);
        }
        if started.elapsed() > HOLDER_START_TIMEOUT {
            return Err(format!(
                "the terminal session holder did not start in time; see {}",
                log_path().display()
            ));
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "`attach` is a plain command-line process with no executor to block"
)]
fn spawn_holder(options: &AttachOptions) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt as _;

    let mut arguments: Vec<OsString> = vec![
        "holder".into(),
        "--id".into(),
        options.id.clone().into(),
        "--detached-timeout-secs".into(),
        options.detached_timeout_secs.to_string().into(),
        "--replay-bytes".into(),
        options.replay_bytes.to_string().into(),
        "--".into(),
    ];
    arguments.extend(options.program.iter().cloned());
    let mut command = holder_command(arguments);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: runs in the forked child before `exec`, and calls only
    // `setsid`, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|error| format!("could not start the terminal session holder: {error}"))
}

#[cfg(not(test))]
fn holder_command(arguments: Vec<OsString>) -> std::process::Command {
    use std::os::unix::process::CommandExt as _;

    let current_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("remote_server"));
    // This very image, not whatever is at its path now: Zed can replace the
    // file (a dev build is uploaded again under the same name), and a holder
    // of another version could not talk to this attach.
    let executable = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        current_exe.clone()
    };
    let mut command = std::process::Command::new(executable);
    command
        .arg0(current_exe)
        .arg("terminal-session")
        .args(arguments);
    command
}

#[cfg(test)]
fn holder_command(arguments: Vec<OsString>) -> std::process::Command {
    tests::subprocess(&arguments)
}

fn terminal_size() -> WindowSize {
    window_size_of(STDIN)
        .or_else(|| window_size_of(STDOUT))
        .unwrap_or(WindowSize::FALLBACK)
}

/// Says who we are, and checks that the holder speaks our protocol.
fn introduce(mut stream: UnixStream, id: &str, size: WindowSize) -> Result<UnixStream, String> {
    let mut hello = encode_preamble(Role::Attach).to_vec();
    ToHolder::Hello {
        size,
        id: id.to_string(),
    }
    .encode(&mut hello);
    let failed = |error: io::Error| format!("the terminal session holder did not answer: {error}");
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(failed)?;
    stream.write_all(&hello).map_err(failed)?;
    let mut answer = [0; PREAMBLE_LEN];
    stream.read_exact(&mut answer).map_err(failed)?;
    let mut decoder = Decoder::default();
    decoder.extend(&answer);
    let preamble = match decoder.take_preamble() {
        Ok(Some(preamble)) => preamble,
        Ok(None) => return Err("the terminal session holder did not answer".to_string()),
        Err(error) => return Err(format!("{error:#}")),
    };
    if preamble.version != PROTOCOL_VERSION {
        return Err(format!(
            "the shell of this terminal is kept by another version of the remote server \
             (protocol {}, this one speaks {PROTOCOL_VERSION}); close the terminal to end it",
            preamble.version
        ));
    }
    stream.set_read_timeout(None).map_err(failed)?;
    Ok(stream)
}

/// The terminal settings to put back when the attach ends.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> Option<Self> {
        // SAFETY: an all-zero `termios` is valid; the calls only read and
        // write it and stdin's settings.
        unsafe {
            if libc::isatty(STDIN) != 1 {
                return None;
            }
            let mut original: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(STDIN, &mut original) != 0 {
                return None;
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(STDIN, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(Self { original })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // `TCSANOW`: waiting for output to drain would never end once the
        // connection is gone.
        // SAFETY: `original` outlives the call, which only reads it.
        unsafe { libc::tcsetattr(STDIN, libc::TCSANOW, &self.original) };
    }
}

fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

/// Copies the holder's output to stdout and stdin to the holder until the
/// shell or the connection ends.
fn relay(mut stream: UnixStream, tell_resumed: bool, nonce: &str, signals: &SignalPipe) -> Outcome {
    // Whatever happens to this end of the connection (stdin or stdout going
    // away is ssh going away) or to the one to the holder, the shell lives
    // on, and the terminal can attach to it again.
    let detached = |reason: &str| Outcome::Detached(reason.to_string());
    if write_stdout(&replay_begin_marker(nonce)).is_err() {
        return detached("could not write to the terminal");
    }
    let mut decoder = Decoder::default();
    let mut buffer = vec![0u8; READ_CHUNK];
    let interests = [
        Interest {
            fd: stream.as_raw_fd(),
            read: true,
            write: false,
        },
        Interest {
            fd: signals.fd(),
            read: true,
            write: false,
        },
        Interest {
            fd: STDIN,
            read: true,
            write: false,
        },
    ];
    loop {
        let readiness = match wait(&interests, None) {
            Ok(readiness) => readiness,
            Err(error) => return Outcome::Fail(format!("waiting for input failed: {error}")),
        };
        let [from_holder, from_signals, from_stdin] = readiness.as_slice() else {
            return Outcome::Fail("waiting for input returned the wrong events".to_string());
        };

        if from_signals.readable {
            for signal in signals.take() {
                if signal == libc::SIGWINCH {
                    let mut resize = Vec::new();
                    ToHolder::Resize(terminal_size()).encode(&mut resize);
                    if stream.write_all(&resize).is_err() {
                        return detached("lost the connection to the terminal session holder");
                    }
                } else {
                    return detached(&format!("received signal {signal}"));
                }
            }
        }

        if from_holder.readable {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    return detached("the terminal session holder closed the connection");
                }
                Ok(read) => decoder.extend(&buffer[..read]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(error) => {
                    return detached(&format!(
                        "lost the connection to the terminal session holder: {error}"
                    ));
                }
            }
            loop {
                let message = match decoder.take_to_attach() {
                    Ok(Some(message)) => message,
                    Ok(None) => break,
                    Err(error) => return detached(&format!("{error:#}")),
                };
                let written = match message {
                    ToAttach::Replay(bytes) | ToAttach::Output(bytes) => write_stdout(&bytes),
                    ToAttach::ReplayEnd => {
                        let mut bytes = replay_end_marker(nonce);
                        if tell_resumed {
                            bytes.extend_from_slice(resumed_without_shell_notice().as_bytes());
                        }
                        write_stdout(&bytes)
                    }
                    ToAttach::ShellExited {
                        status,
                        while_detached,
                    } => {
                        if while_detached {
                            write_stdout(shell_exited_while_detached_notice(status).as_bytes())
                                .ok();
                        }
                        return Outcome::Exit(status.attach_exit_code());
                    }
                    ToAttach::TakenOver => {
                        write_stdout(taken_over_notice().as_bytes()).ok();
                        return Outcome::Exit(EXIT_TAKEN_OVER);
                    }
                    ToAttach::Error(reason) => return Outcome::Fail(reason),
                };
                if written.is_err() {
                    return detached("could not write to the terminal");
                }
            }
        }

        if from_stdin.readable {
            // SAFETY: `buffer` outlives the call and is as long as we say.
            let read = unsafe { libc::read(STDIN, buffer.as_mut_ptr().cast(), buffer.len()) };
            if read > 0 {
                let mut input = Vec::new();
                ToHolder::Input(buffer[..read as usize].to_vec()).encode(&mut input);
                if stream.write_all(&input).is_err() {
                    return detached("lost the connection to the terminal session holder");
                }
            } else if read == 0
                || !matches!(
                    last_error().kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                )
            {
                return detached("the terminal closed its input");
            }
        }
    }
}

/// Ends the shell kept for `id`, if there is one.
pub(super) fn kill_session(directory: &Path, id: &str) -> Result<()> {
    let paths = SessionPaths::resolve(directory, id, false)?;
    let mut stream = match UnixStream::connect(&paths.socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error).context("reaching the terminal session holder"),
    };
    stream.set_read_timeout(Some(KILL_TIMEOUT))?;
    stream.set_write_timeout(Some(KILL_TIMEOUT))?;
    stream
        .write_all(&encode_preamble(Role::Kill))
        .context("asking the terminal session holder to end")?;
    let mut answer = [0; PREAMBLE_LEN];
    match stream.read_exact(&mut answer) {
        Ok(()) => Ok(()),
        // A holder already on its way out closes without answering.
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
        Err(error) => Err(error).context("the terminal session holder did not answer"),
    }
}

#[cfg(test)]
mod tests;
