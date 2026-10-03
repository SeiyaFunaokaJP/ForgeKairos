//! The proxy itself: listener, connection handlers, upstream chaining.
//!
//! All synchronous, thread-per-connection. `ProxyHandle::spawn` binds a
//! `std::net::TcpListener` on `127.0.0.1:0` and returns once the listener
//! is bound and the listener thread has been spawned. Drop the handle to
//! shut everything down — the listener thread stops accepting new
//! connections; in-flight connection threads finish on their own when
//! either side closes.
//!
//! See the crate-level docs for trust assumptions and the "no proxy here"
//! principle.

mod connection;
mod upstream;

use crate::allowlist::Allowlist;
use anyhow::{Context, Result};
use futures::channel::mpsc;
use std::fmt;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

/// Cap on concurrently handled connections. Each connection costs the
/// editor process two threads and two pump buffers; the cap keeps a
/// runaway (or malicious) sandboxed command from exhausting the editor's
/// thread/fd budget. Well above what parallel package managers open.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

pub use upstream::UpstreamProxy;

/// Configuration for spawning a proxy.
#[derive(Clone)]
pub struct ProxyConfig {
    /// Hosts the proxy will allow to be reached. Ignored when `policy` is set.
    pub allowlist: Allowlist,
    /// Optional upstream HTTP proxy to chain through, with `NO_PROXY`-style
    /// bypasses for hosts that should connect direct.
    pub upstream: Option<UpstreamProxy>,
    /// Where the proxy reports per-connection events. Use
    /// [`mpsc::unbounded`] so connection threads (which are sync) never
    /// block on send. The receiver is async-friendly so `gpui` / `tokio`
    /// callers can poll it from their executor of choice.
    pub events: mpsc::UnboundedSender<ProxyEvent>,
    /// Replaces `allowlist` and the DNS-rebinding filter for this instance.
    /// `None` keeps the sandbox policy described on [`DestinationPolicy`].
    pub policy: Option<Arc<dyn DestinationPolicy>>,
    /// An outer policy every destination must satisfy *in addition to* the
    /// decision `allowlist` (or `policy`) reached — the two are ANDed, never
    /// unioned, so this can only narrow.
    ///
    /// This is how one policy nests inside another: the inner decision is the
    /// smaller box, this is the ceiling around it. This fork uses it to put the
    /// terminal sandbox's own host allowlist inside the editor-wide egress
    /// allowlist, so a host the user approved for one sandboxed command still
    /// cannot be reached unless the editor is allowed to reach it at all.
    pub outer_policy: Option<Arc<dyn DestinationPolicy>>,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("allowlist", &self.allowlist)
            .field("upstream", &self.upstream)
            .field("policy", &self.policy.as_ref().map(|_| "<custom>"))
            .field(
                "outer_policy",
                &self.outer_policy.as_ref().map(|_| "<custom>"),
            )
            .finish_non_exhaustive()
    }
}

/// A destination policy supplied by the caller, replacing both the
/// [`Allowlist`] check and the DNS-rebinding filter for one proxy instance.
///
/// The built-in policy is written for the sandbox: its only client is
/// model-driven code running inside an OS sandbox, where an IP-literal target
/// or a hostname resolving into loopback / private space is an escape attempt
/// rather than a destination. A caller enforcing a *different* policy supplies
/// it here instead of loosening that one. This fork points ACP agent, MCP and
/// npm child processes at the editor's own egress allowlist
/// (`.forgekairos-docs/DESIGN.md` ADR-F13), which deliberately does permit IP
/// literals and loopback so that a local LLM server stays reachable.
///
/// The same trait also fills [`ProxyConfig::outer_policy`], where an
/// implementation *narrows* the base decision instead of replacing it.
pub trait DestinationPolicy: Send + Sync + 'static {
    fn evaluate(&self, host: &str, port: u16) -> PolicyDecision;

    /// Whether one address the destination resolved to is out of reach whatever
    /// [`Self::evaluate`] decided about the destination itself.
    ///
    /// Asked for every address the proxy is about to connect to, after
    /// resolution and after [`Self::evaluate`], so that a name resolving to a
    /// refused address — the answer changed between the two (DNS rebinding), or
    /// a name that always pointed there — is refused as well. A policy whose
    /// decision needs no more than the destination's name leaves this alone; the
    /// sandbox's own policy uses it to keep a command off the addresses the host
    /// it runs on holds, which no list of host patterns can name.
    fn forbids_address(&self, _address: std::net::IpAddr) -> bool {
        false
    }
}

/// What a [`DestinationPolicy`] decided about one destination.
pub enum PolicyDecision {
    /// Reachable. `permit_local_addresses` waives the DNS-rebinding filter,
    /// for policies under which loopback / private destinations are legitimate.
    Allow { permit_local_addresses: bool },
    /// Refused. The reason reaches both the client and the event stream.
    Deny(DenyReason),
}

/// A request method seen by the proxy.
///
/// Either a CONNECT (HTTPS tunnel) or an HTTP forward request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestMethod {
    Connect,
    Http(String),
}

impl RequestMethod {
    pub fn as_str(&self) -> &str {
        match self {
            RequestMethod::Connect => "CONNECT",
            RequestMethod::Http(method) => method.as_str(),
        }
    }
}

/// Outcome of a single connection's policy decision.
#[derive(Debug, Clone)]
pub enum RequestOutcome {
    Allowed,
    Denied { reason: DenyReason },
}

/// Why an attempted connection was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// Hostname (in punycode form on the wire) wasn't in the allowlist.
    HostNotInAllowlist { host: String },
    /// CONNECT or HTTP request targeted an IP literal. Denied unless the
    /// allowlist allows any host.
    IpLiteralRejected { target: String },
    /// The hostname resolved only to loopback / private / link-local
    /// addresses, which the sandbox policy never reaches via the allowlist
    /// (DNS-rebinding protection). Not applied when the allowlist allows
    /// any host.
    ResolvedToForbiddenIp { host: String },
    /// A caller-supplied [`DestinationPolicy`] refused the destination.
    /// `explanation` is what the client is told.
    NotPermittedByPolicy { host: String, explanation: String },
}

impl DenyReason {
    pub(crate) fn proxy_status_error(&self) -> &'static str {
        match self {
            DenyReason::HostNotInAllowlist { .. } => "destination_ip_prohibited",
            DenyReason::IpLiteralRejected { .. } => "destination_ip_prohibited",
            DenyReason::ResolvedToForbiddenIp { .. } => "destination_ip_prohibited",
            DenyReason::NotPermittedByPolicy { .. } => "destination_ip_prohibited",
        }
    }

    pub(crate) fn human_explanation(&self) -> String {
        match self {
            DenyReason::HostNotInAllowlist { host } => {
                format!("host '{host}' is not in this conversation's network allowlist")
            }
            DenyReason::IpLiteralRejected { target } => format!(
                "target '{target}' is an IP literal; only hostnames are permitted by sandbox policy"
            ),
            DenyReason::ResolvedToForbiddenIp { host } => format!(
                "host '{host}' resolves only to loopback/private/link-local addresses, \
                 which sandbox policy blocks"
            ),
            DenyReason::NotPermittedByPolicy { explanation, .. } => explanation.clone(),
        }
    }
}

/// Events emitted by the proxy as it handles connections.
#[derive(Debug, Clone)]
pub enum ProxyEvent {
    /// Sent once after the listener is bound. Always the first event for
    /// a given proxy instance.
    Ready { port: u16 },

    /// Emitted at policy-decision time, before bytes flow to the upstream.
    RequestAttempt {
        host: String,
        port: u16,
        method: RequestMethod,
        outcome: RequestOutcome,
    },

    /// Emitted after an `Allowed` connection finishes. Carries throughput
    /// totals for diagnostics. Not emitted for denied connections.
    RequestCompleted {
        host: String,
        port: u16,
        method: RequestMethod,
        bytes_to_remote: u64,
        bytes_from_remote: u64,
        duration_ms: u64,
    },
}

/// Handle to a running proxy. Drop to stop the listener; in-flight
/// connection threads finish on their own as soon as either side closes.
pub struct ProxyHandle {
    port: u16,
    socket_path: Option<PathBuf>,
    cleanup_directory: Option<PathBuf>,
    /// Listener thread sees this flip to `true` after `accept` returns and
    /// then exits.
    shutdown: Arc<AtomicBool>,
    /// Joined on drop to make shutdown deterministic in tests; ignored if
    /// the listener has already exited.
    listener_thread: Option<thread::JoinHandle<()>>,
}

impl ProxyHandle {
    /// Spawns the proxy: binds a listener on `127.0.0.1:0`, spawns the
    /// listener thread, sends a `Ready` event, and returns. The returned
    /// port is what callers should use for `HTTPS_PROXY`/`HTTP_PROXY` env
    /// vars and for the seatbelt rule narrowing `localhost:<port>`.
    pub fn spawn(config: ProxyConfig) -> Result<ProxyHandle> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("failed to bind proxy listener on 127.0.0.1:0")?;
        let port = listener
            .local_addr()
            .context("failed to read proxy local addr")?
            .port();

        // Inform the parent the proxy is ready before starting the accept
        // loop. Send is fire-and-forget on an unbounded channel — never
        // blocks, never errors meaningfully.
        let _ = config.events.unbounded_send(ProxyEvent::Ready { port });

        let shutdown = Arc::new(AtomicBool::new(false));
        let runtime_state = Arc::new(RuntimeState {
            allowlist: config.allowlist,
            upstream: config.upstream,
            events: config.events,
            policy: config.policy,
            outer_policy: config.outer_policy,
            active_connections: AtomicUsize::new(0),
        });

        let listener_thread = thread::Builder::new()
            .name("http-proxy-listener".to_string())
            // Listener thread does almost nothing on its stack — accept,
            // spawn, loop. 128 KiB is plenty.
            .stack_size(128 * 1024)
            .spawn({
                let shutdown = shutdown.clone();
                move || run_listener(listener, runtime_state, shutdown)
            })
            .context("failed to spawn proxy listener thread")?;

        Ok(ProxyHandle {
            port,
            socket_path: None,
            cleanup_directory: None,
            shutdown,
            listener_thread: Some(listener_thread),
        })
    }

    /// Spawns the proxy on a fresh pathname Unix socket under the system temp
    /// directory and reserves a loopback port number for the in-sandbox bridge.
    #[cfg(unix)]
    pub fn spawn_unix_temp(config: ProxyConfig) -> Result<ProxyHandle> {
        let directory = unique_temp_socket_directory();
        let path = directory.join("proxy.sock");
        Self::spawn_unix_with_cleanup(path, Some(directory), config)
    }

    /// Spawns the proxy on a pathname Unix socket and reserves a loopback port
    /// number for the in-sandbox bridge to listen on.
    #[cfg(unix)]
    pub fn spawn_unix(path: impl AsRef<Path>, config: ProxyConfig) -> Result<ProxyHandle> {
        Self::spawn_unix_with_cleanup(path.as_ref().to_path_buf(), None, config)
    }

    #[cfg(unix)]
    fn spawn_unix_with_cleanup(
        path: PathBuf,
        cleanup_directory: Option<PathBuf>,
        config: ProxyConfig,
    ) -> Result<ProxyHandle> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create proxy socket directory {}",
                    parent.display()
                )
            })?;
        }
        if path.exists() {
            std::fs::remove_file(&path).with_context(|| {
                format!("failed to remove stale proxy socket {}", path.display())
            })?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("failed to bind proxy Unix socket {}", path.display()))?;
        let port = reserve_loopback_port()?;

        let _ = config.events.unbounded_send(ProxyEvent::Ready { port });

        let shutdown = Arc::new(AtomicBool::new(false));
        let runtime_state = Arc::new(RuntimeState {
            allowlist: config.allowlist,
            upstream: config.upstream,
            events: config.events,
            policy: config.policy,
            outer_policy: config.outer_policy,
            active_connections: AtomicUsize::new(0),
        });

        let listener_thread = thread::Builder::new()
            .name("http-proxy-unix-listener".to_string())
            .stack_size(128 * 1024)
            .spawn({
                let shutdown = shutdown.clone();
                move || run_unix_listener(listener, runtime_state, shutdown)
            })
            .context("failed to spawn proxy Unix listener thread")?;

        Ok(ProxyHandle {
            port,
            socket_path: Some(path),
            cleanup_directory,
            shutdown,
            listener_thread: Some(listener_thread),
        })
    }

    /// The loopback TCP port clients should use for proxy environment variables.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Path of the Unix socket listener, for Linux bridge mode.
    pub fn socket_path(&self) -> Option<&Path> {
        self.socket_path.as_deref()
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // The listener is blocked in `accept()`. Waking it up cleanly via
        // a flag alone isn't possible with `std::net::TcpListener` — there's
        // no way to interrupt the syscall. Connect to ourselves: the
        // listener wakes up, accepts the connection, sees the shutdown
        // flag, breaks the loop. The accepted connection's worker thread
        // will read the empty stream and exit too.
        if let Some(path) = &self.socket_path {
            #[cfg(unix)]
            let _ = UnixStream::connect(path);
            #[cfg(not(unix))]
            let _ = path;
        } else {
            let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        }

        if let Some(thread) = self.listener_thread.take() {
            // Give the listener a chance to clean up. A join error means the
            // listener thread panicked; there's nothing to recover, but it
            // shouldn't pass unnoticed.
            if thread.join().is_err() {
                log::warn!("[http_proxy] listener thread panicked");
            }
        }
        if let Some(path) = &self.socket_path
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!(
                "[http_proxy] failed to remove proxy Unix socket {}: {error}",
                path.display()
            );
        }
        if let Some(directory) = &self.cleanup_directory
            && let Err(error) = std::fs::remove_dir(directory)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!(
                "[http_proxy] failed to remove proxy socket directory {}: {error}",
                directory.display()
            );
        }
    }
}

#[cfg(unix)]
fn unique_temp_socket_directory() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "zed-proxy-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

/// State shared across all connection threads for a single proxy instance.
pub(crate) struct RuntimeState {
    pub(crate) allowlist: Allowlist,
    pub(crate) upstream: Option<UpstreamProxy>,
    pub(crate) events: mpsc::UnboundedSender<ProxyEvent>,
    pub(crate) policy: Option<Arc<dyn DestinationPolicy>>,
    pub(crate) outer_policy: Option<Arc<dyn DestinationPolicy>>,
    active_connections: AtomicUsize,
}

/// Decrements the active-connection count when a connection thread finishes
/// (normally or by panic).
struct ConnectionSlot(Arc<RuntimeState>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn reserve_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("failed to reserve proxy bridge port")?;
    Ok(listener
        .local_addr()
        .context("failed to read reserved proxy bridge port")?
        .port())
}

fn run_listener(listener: TcpListener, state: Arc<RuntimeState>, shutdown: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            log::debug!("[http_proxy] listener stopping (shutdown signaled)");
            break;
        }
        match stream {
            Ok(stream) => spawn_connection(connection::ClientStream::Tcp(stream), &state),
            Err(e) => {
                // EMFILE / per-process fd exhaustion is the realistic
                // failure here. Log and keep going — accept errors are
                // usually transient.
                log::warn!("[http_proxy] accept failed: {e}");
            }
        }
    }
}

#[cfg(unix)]
fn run_unix_listener(listener: UnixListener, state: Arc<RuntimeState>, shutdown: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            log::debug!("[http_proxy] Unix listener stopping (shutdown signaled)");
            break;
        }
        match stream {
            Ok(stream) => spawn_connection(connection::ClientStream::Unix(stream), &state),
            Err(error) => log::warn!("[http_proxy] Unix accept failed: {error}"),
        }
    }
}

fn spawn_connection(stream: connection::ClientStream, state: &Arc<RuntimeState>) {
    let previous = state.active_connections.fetch_add(1, Ordering::SeqCst);
    if previous >= MAX_CONCURRENT_CONNECTIONS {
        state.active_connections.fetch_sub(1, Ordering::SeqCst);
        log::warn!(
            "[http_proxy] dropping connection: {MAX_CONCURRENT_CONNECTIONS} \
             connections already active"
        );
        drop(stream);
        return;
    }
    let slot = ConnectionSlot(state.clone());
    let state = state.clone();
    let result = thread::Builder::new()
        .name("http-proxy-conn".to_string())
        .stack_size(128 * 1024)
        .spawn(move || {
            let _slot = slot;
            if let Err(error) = connection::handle(stream, state) {
                log::debug!("[http_proxy] connection handler error: {error}");
            }
        });
    if let Err(error) = result {
        log::warn!("[http_proxy] failed to spawn connection thread: {error}");
    }
}
