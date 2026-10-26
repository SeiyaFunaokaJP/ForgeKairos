//! ADR-F45: one `remote_server` process — the daemon — serves every session a
//! client has on this host. [`SessionRegistry`] owns those sessions, and
//! [`serve`] admits the connections that `remote_server proxy` opens to it.

use crate::headless_project::{HeadlessHost, HeadlessProject, HeadlessProjectEvent};
use anyhow::{Result, anyhow};
use collections::HashMap;
use futures::{
    AsyncRead, AsyncWrite, AsyncWriteExt as _, Future, FutureExt as _, SinkExt as _,
    StreamExt as _,
    channel::{mpsc, oneshot},
    future::Fuse,
    io::WriteHalf,
    select_biased,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, FutureExt as _, Subscription, Task, WeakEntity,
};
use net::async_net::UnixListener;
use remote::{
    ProxyHttpRelay, RemoteClient,
    protocol::{
        DAEMON_PROTOCOL_VERSION, DaemonFrameKind, DaemonHello, DaemonHelloOutcome,
        DaemonHelloReply, decode_envelope, read_daemon_frame, read_json_message,
        write_daemon_frame, write_envelope_frame, write_json_message,
    },
};
use rpc::{AnyProtoClient, proto::Envelope};
use std::{
    path::Path,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

/// A session left without a connection this long is closed. The client stops
/// trying to reconnect well before that (ADR-F21), so nothing would resume it.
pub(crate) const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a daemon with no session left waits before exiting, so that closing
/// the last window and opening another soon after still finds it running.
pub(crate) const EMPTY_DAEMON_LINGER: Duration = Duration::from_secs(60);

/// How long the daemon waits for a fresh connection to introduce itself.
pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a write to a proxy may take before its connection is given up on.
/// A proxy whose ssh session died without anyone noticing stops reading, and
/// the write would otherwise wait for it — and keep its session from ever
/// idling out — for as long as the kernel keeps the socket.
const STALLED_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a session the client closed gets to put what it has already queued
/// — the acknowledgement of `ShutdownRemoteServer` among it — on the wire.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

const MIN_ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
const MAX_ACCEPT_BACKOFF: Duration = Duration::from_secs(2);

pub(crate) trait DaemonStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> DaemonStream for T {}

/// A connection from a proxy to this daemon.
pub(crate) type DaemonConnection = Box<dyn DaemonStream>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionId(u64);

/// The sessions that the daemon's process-wide senders can reach: telemetry,
/// the ADR-F27 HTTP relay, settings errors and the log stream all have one
/// process behind them now, but a connection per session to go out on.
#[derive(Clone, Default)]
pub(crate) struct SessionRouter(Arc<Mutex<RouterState>>);

#[derive(Default)]
struct RouterState {
    sessions: Vec<RoutedSession>,
    connections_made: u64,
}

struct RoutedSession {
    id: SessionId,
    client: AnyProtoClient,
    log_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Orders the sessions by when their current connection was made; `None`
    /// while a session has none.
    connection: Option<u64>,
    /// The agent socket of the ssh session behind this session's newest
    /// connection.
    ssh_auth_sock: Option<String>,
}

fn most_recently_connected(sessions: &[RoutedSession]) -> Option<&RoutedSession> {
    sessions
        .iter()
        .filter(|session| session.connection.is_some())
        .max_by_key(|session| session.connection)
}

impl SessionRouter {
    fn state(&self) -> MutexGuard<'_, RouterState> {
        // Every update leaves the state whole, so a panic elsewhere while it
        // was held is no reason to stop routing.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn add(&self, id: SessionId, client: AnyProtoClient, log_tx: mpsc::UnboundedSender<Vec<u8>>) {
        self.state().sessions.push(RoutedSession {
            id,
            client,
            log_tx,
            connection: None,
            ssh_auth_sock: None,
        });
    }

    fn remove(&self, id: SessionId) {
        self.state().sessions.retain(|session| session.id != id);
        self.follow_ssh_agent();
    }

    fn set_ssh_auth_sock(&self, id: SessionId, ssh_auth_sock: Option<String>) {
        if let Some(session) = self
            .state()
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
        {
            session.ssh_auth_sock = ssh_auth_sock;
        }
    }

    fn set_connected(&self, id: SessionId, connected: bool) {
        {
            let mut state = self.state();
            let connection = if connected {
                state.connections_made += 1;
                Some(state.connections_made)
            } else {
                None
            };
            if let Some(session) = state.sessions.iter_mut().find(|session| session.id == id) {
                session.connection = connection;
            }
        }
        self.follow_ssh_agent();
    }

    /// The session most recently connected; failing that, the one most
    /// recently opened, whose client buffers what is sent until it reconnects.
    /// Any of them will do for what goes through here, since every session of
    /// a daemon belongs to the same IDE.
    pub(crate) fn preferred_client(&self) -> Option<AnyProtoClient> {
        let state = self.state();
        most_recently_connected(&state.sessions)
            .or_else(|| state.sessions.last())
            .map(|session| session.client.clone())
    }

    /// Only a session that has a connection right now: a caller waiting for an
    /// answer would otherwise wait for a client that may never come back.
    pub(crate) fn connected_client(&self) -> Option<AnyProtoClient> {
        most_recently_connected(&self.state().sessions).map(|session| session.client.clone())
    }

    pub(crate) fn all_clients(&self) -> Vec<AnyProtoClient> {
        self.state()
            .sessions
            .iter()
            .map(|session| session.client.clone())
            .collect()
    }

    /// Where the process's log goes: the most recently connected session
    /// only, so that a client with several windows on this host does not get
    /// every line once per window.
    fn log_sink(&self) -> Option<mpsc::UnboundedSender<Vec<u8>>> {
        most_recently_connected(&self.state().sessions).map(|session| session.log_tx.clone())
    }

    /// The agent socket of the most recently connected session that still has
    /// one on disk.
    fn ssh_agent_to_follow(&self) -> Option<String> {
        let state = self.state();
        let mut connected = state
            .sessions
            .iter()
            .filter(|session| session.connection.is_some())
            .collect::<Vec<_>>();
        connected.sort_by_key(|session| std::cmp::Reverse(session.connection));
        connected.into_iter().find_map(|session| {
            session
                .ssh_auth_sock
                .clone()
                .filter(|socket| Path::new(socket).exists())
        })
    }

    /// Points this process's `SSH_AUTH_SOCK` — which the git and ssh it runs
    /// for every session inherit — at the agent of the most recently connected
    /// session.
    ///
    /// The process starts with the agent of whichever proxy started it, and
    /// that agent goes away with its ssh session. Giving each session its own
    /// agent would need every place that spawns a process to take its
    /// environment from the session; until then, the newest connection's
    /// agent is the one most likely to still be there.
    fn follow_ssh_agent(&self) {
        let Some(agent) = self.ssh_agent_to_follow() else {
            return;
        };
        if std::env::var_os("SSH_AUTH_SOCK").is_some_and(|current| current == agent.as_str()) {
            return;
        }
        log::info!("using the ssh agent at {agent} for the git and ssh this host runs");
        // SAFETY: the same process-wide update `util::load_login_shell_environment`
        // makes after startup; spawned processes read the environment when
        // they start.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &agent) };
    }
}

pub(crate) struct SessionRegistryOptions {
    pub host: Entity<HeadlessHost>,
    pub router: SessionRouter,
    pub proxy_http_relay: Entity<ProxyHttpRelay>,
    pub is_wsl_interop: bool,
    pub init_worktree_trust: bool,
    /// Runs once the daemon has had no session for [`EMPTY_DAEMON_LINGER`].
    pub on_idle: Rc<dyn Fn(&mut App)>,
}

/// Every session this daemon serves, by the identifier its client gave it.
pub(crate) struct SessionRegistry {
    host: Entity<HeadlessHost>,
    router: SessionRouter,
    proxy_http_relay: Entity<ProxyHttpRelay>,
    is_wsl_interop: bool,
    init_worktree_trust: bool,
    on_idle: Rc<dyn Fn(&mut App)>,
    sessions: HashMap<String, Session>,
    next_session_id: u64,
    idle_exit: Option<Task<()>>,
    _log_forwarding: Task<()>,
}

struct Session {
    id: SessionId,
    project: Entity<HeadlessProject>,
    connections: mpsc::UnboundedSender<DaemonConnection>,
    drain: oneshot::Sender<()>,
    pump: Task<()>,
    _shutdown_requests: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closing {
    /// The client asked for it: what the session has already queued — the
    /// acknowledgement among it — still goes out first.
    Drain,
    Now,
}

/// What a proxy's introduction gets it.
pub(crate) enum Admission {
    /// Hand the connection to the session through this.
    Accepted(mpsc::UnboundedSender<DaemonConnection>),
    SessionNotFound,
}

impl SessionRegistry {
    pub(crate) fn new(
        options: SessionRegistryOptions,
        log_rx: smol::channel::Receiver<Vec<u8>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let log_forwarding = cx.background_spawn({
            let router = options.router.clone();
            async move {
                while let Ok(chunk) = log_rx.recv().await {
                    if let Some(sink) = router.log_sink() {
                        sink.unbounded_send(chunk).ok();
                    }
                }
            }
        });
        let mut this = Self {
            host: options.host,
            router: options.router,
            proxy_http_relay: options.proxy_http_relay,
            is_wsl_interop: options.is_wsl_interop,
            init_worktree_trust: options.init_worktree_trust,
            on_idle: options.on_idle,
            sessions: HashMap::default(),
            next_session_id: 0,
            idle_exit: None,
            _log_forwarding: log_forwarding,
        };
        // The proxy that started this daemon may die before it introduces
        // itself, and nothing else would ever end the process then.
        this.schedule_exit_if_idle(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn session_count(&self) -> usize {
        self.sessions.len()
    }

    #[cfg(test)]
    pub(crate) fn project(&self, identifier: &str) -> Option<Entity<HeadlessProject>> {
        self.sessions
            .get(identifier)
            .map(|session| session.project.clone())
    }

    pub(crate) fn admit(&mut self, hello: &DaemonHello, cx: &mut Context<Self>) -> Admission {
        let identifier = hello.identifier.as_str();
        let admission = if hello.reconnect {
            match self.sessions.get(identifier) {
                Some(session) => {
                    session
                        .project
                        .update(cx, |project, cx| project.connection_restored(cx));
                    Admission::Accepted(session.connections.clone())
                }
                None => Admission::SessionNotFound,
            }
        } else {
            if self.sessions.contains_key(identifier) {
                // A client starts a session afresh only when it holds no state
                // for it, so whatever this daemon still keeps under that name
                // is stale.
                log::info!("[{identifier}] a fresh connection starts this session over");
                self.close_session(identifier, Closing::Now, cx);
            }
            Admission::Accepted(self.open_session(identifier, cx))
        };
        if matches!(admission, Admission::Accepted(_))
            && let Some(session) = self.sessions.get(identifier)
        {
            self.router
                .set_ssh_auth_sock(session.id, hello.ssh_auth_sock.clone());
        }
        admission
    }

    fn open_session(
        &mut self,
        identifier: &str,
        cx: &mut Context<Self>,
    ) -> mpsc::UnboundedSender<DaemonConnection> {
        let id = SessionId(self.next_session_id);
        self.next_session_id += 1;

        let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded::<Envelope>();
        // Serves nothing but pings until the client has shown this build's
        // wire revision.
        let client = RemoteClient::server_proto_client_from_channels(
            incoming_rx,
            outgoing_tx,
            cx,
            "server",
            self.is_wsl_interop,
        );
        ProxyHttpRelay::attach(&self.proxy_http_relay, &client);
        let project = cx.new({
            let host = self.host.clone();
            let client = client.clone();
            let init_worktree_trust = self.init_worktree_trust;
            move |cx| HeadlessProject::new_in_host(host, client, init_worktree_trust, cx)
        });
        crate::handle_crash_files_requests(&project, &client);
        let shutdown_requests = cx.subscribe(
            &project,
            move |this, _, event: &HeadlessProjectEvent, cx| match event {
                HeadlessProjectEvent::ShutdownRequested => {
                    this.close_session_with_id(id, Closing::Drain, cx)
                }
            },
        );

        let (connections_tx, connections_rx) = mpsc::unbounded();
        let (drain_tx, drain_rx) = oneshot::channel();
        let (log_tx, log_rx) = mpsc::unbounded();
        self.router.add(id, client, log_tx);
        let pump = cx.spawn({
            let pump = SessionPump {
                identifier: identifier.to_string(),
                id,
                connections: connections_rx,
                drain: drain_rx.fuse(),
                incoming_tx,
                outgoing_rx,
                log_rx,
                router: self.router.clone(),
            };
            async move |this, cx| {
                pump_session(pump, cx).await;
                // Whichever way the pump ended, the session is over. Closing it
                // again is harmless when the registry already has.
                this.update(cx, |this, cx| {
                    this.close_session_with_id(id, Closing::Now, cx)
                })
                .ok();
            }
        });

        self.sessions.insert(
            identifier.to_string(),
            Session {
                id,
                project,
                connections: connections_tx.clone(),
                drain: drain_tx,
                pump,
                _shutdown_requests: shutdown_requests,
            },
        );
        self.idle_exit = None;
        log::info!(
            "[{identifier}] opened a session ({} open)",
            self.sessions.len()
        );
        connections_tx
    }

    fn close_session_with_id(&mut self, id: SessionId, closing: Closing, cx: &mut Context<Self>) {
        let identifier = self
            .sessions
            .iter()
            .find_map(|(identifier, session)| (session.id == id).then(|| identifier.clone()));
        if let Some(identifier) = identifier {
            self.close_session(&identifier, closing, cx);
        }
    }

    fn close_session(&mut self, identifier: &str, closing: Closing, cx: &mut Context<Self>) {
        let Some(Session {
            id,
            project,
            connections,
            drain,
            pump,
            ..
        }) = self.sessions.remove(identifier)
        else {
            return;
        };
        self.router.remove(id);

        let draining = closing == Closing::Drain && drain.send(()).is_ok();
        if draining {
            // The connection sender stays open until the pump is done, or the
            // pump would take the closed channel for the session ending there
            // and then, before it has flushed anything.
            cx.spawn(async move |_, _| {
                pump.await;
                drop(connections);
            })
            .detach();
        } else {
            drop(pump);
            drop(connections);
        }

        let shutdown = project.update(cx, |project, cx| project.shutdown(cx));
        cx.spawn(async move |_, cx| {
            shutdown.await;
            // Held until the language servers have stopped, so that dropping
            // the store that talks to them does not cut their exit short.
            // Dropped inside an update: GPUI releases an entity whose last
            // handle is gone only when an update ends, and outside one the
            // project's worktrees, watches and stores would linger until some
            // unrelated update happened to come along.
            cx.update(|_| drop(project));
        })
        .detach();
        log::info!(
            "[{identifier}] closed the session ({} open)",
            self.sessions.len()
        );
        self.schedule_exit_if_idle(cx);
    }

    fn schedule_exit_if_idle(&mut self, cx: &mut Context<Self>) {
        if !self.sessions.is_empty() {
            self.idle_exit = None;
            return;
        }
        self.idle_exit = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(EMPTY_DAEMON_LINGER).await;
            let on_idle = this
                .update(cx, |this, _| {
                    this.sessions.is_empty().then(|| this.on_idle.clone())
                })
                .ok()
                .flatten();
            // Run outside the registry's update: exiting tears the app down,
            // the registry included.
            if let Some(on_idle) = on_idle {
                log::info!("no session for {EMPTY_DAEMON_LINGER:?}; exiting");
                cx.update(|cx| on_idle(cx));
            }
        }));
    }
}

struct SessionPump {
    identifier: String,
    id: SessionId,
    connections: mpsc::UnboundedReceiver<DaemonConnection>,
    drain: Fuse<oneshot::Receiver<()>>,
    incoming_tx: mpsc::UnboundedSender<Envelope>,
    outgoing_rx: mpsc::UnboundedReceiver<Envelope>,
    log_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    router: SessionRouter,
}

enum Waited {
    Connection(Option<DaemonConnection>),
    Drain,
    Idle,
}

enum ConnectionEnd {
    Replaced(DaemonConnection),
    Lost,
    SessionClosed,
}

/// Carries one session's traffic over whichever connection it currently has.
///
/// A proxy whose ssh session died without sshd noticing (the client machine
/// suspended and its IP or NAT mapping changed; `ClientAliveInterval` is
/// usually 0 and TCP keepalive defaults to hours) keeps its connection open,
/// so it never reaches EOF. The newest connection is served instead of waiting
/// for the current one to end (ADR-F21), otherwise a reconnecting client would
/// sit unanswered until the stale ssh session eventually died.
async fn pump_session(mut pump: SessionPump, cx: &mut AsyncApp) {
    let mut replacement: Option<DaemonConnection> = None;
    loop {
        let connection = match replacement.take() {
            Some(connection) => connection,
            None => {
                let idle = cx.background_executor().timer(SESSION_IDLE_TIMEOUT);
                // Biased, so that a connection arriving together with the
                // timeout still counts.
                let waited = select_biased! {
                    connection = pump.connections.next().fuse() => Waited::Connection(connection),
                    _ = &mut pump.drain => Waited::Drain,
                    _ = idle.fuse() => Waited::Idle,
                };
                match waited {
                    Waited::Connection(Some(connection)) => connection,
                    Waited::Connection(None) | Waited::Drain => return,
                    Waited::Idle => {
                        log::warn!(
                            "[{}] no connection for {SESSION_IDLE_TIMEOUT:?}; closing the session",
                            pump.identifier
                        );
                        return;
                    }
                }
            }
        };

        pump.router.set_connected(pump.id, true);
        log::info!("[{}] serving a new connection", pump.identifier);
        let end = serve_connection(&mut pump, connection, cx).await;
        pump.router.set_connected(pump.id, false);
        match end {
            ConnectionEnd::Replaced(connection) => {
                log::info!(
                    "[{}] replacing stale connection with a newly accepted one",
                    pump.identifier
                );
                replacement = Some(connection);
            }
            ConnectionEnd::Lost => log::info!(
                "[{}] connection lost; waiting for the client to reconnect",
                pump.identifier
            ),
            ConnectionEnd::SessionClosed => return,
        }
    }
}

enum ConnectionEvent {
    Connection(Option<DaemonConnection>),
    Drain,
    Received(Option<Envelope>),
    Outgoing(Option<Envelope>),
    Log(Option<Vec<u8>>),
}

enum WriteOutcome {
    Written,
    Replaced(Option<DaemonConnection>),
    Failed(anyhow::Error),
}

async fn serve_connection(
    pump: &mut SessionPump,
    connection: DaemonConnection,
    cx: &mut AsyncApp,
) -> ConnectionEnd {
    let (mut reader, mut writer) = futures::io::AsyncReadExt::split(connection);
    let (mut received_tx, mut received_rx) = mpsc::unbounded::<Envelope>();
    // Held rather than detached, so that the reader stops as soon as this
    // connection ends or is replaced instead of lingering on a socket that may
    // never report an error.
    let _reader = cx.background_spawn({
        let identifier = pump.identifier.clone();
        async move {
            let mut buffer = Vec::new();
            loop {
                match read_daemon_frame(&mut reader, &mut buffer).await {
                    Ok(DaemonFrameKind::Envelope) => match decode_envelope(&buffer) {
                        Ok(envelope) => {
                            if received_tx.send(envelope).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            log::warn!(
                                "[{identifier}] dropping a connection that sent an undecodable message: {error:#}"
                            );
                            break;
                        }
                    },
                    Ok(DaemonFrameKind::Log) => {
                        log::warn!("[{identifier}] ignoring a log frame sent by the proxy");
                    }
                    Err(error) => {
                        log::info!("[{identifier}] connection closed: {error:#}");
                        break;
                    }
                }
            }
        }
    });

    let mut output_buffer = Vec::new();
    loop {
        let event = select_biased! {
            connection = pump.connections.next().fuse() => ConnectionEvent::Connection(connection),
            _ = &mut pump.drain => ConnectionEvent::Drain,
            received = received_rx.next().fuse() => ConnectionEvent::Received(received),
            outgoing = pump.outgoing_rx.next().fuse() => ConnectionEvent::Outgoing(outgoing),
            chunk = pump.log_rx.next().fuse() => ConnectionEvent::Log(chunk),
        };
        let written = match event {
            ConnectionEvent::Connection(Some(connection)) => {
                return ConnectionEnd::Replaced(connection);
            }
            ConnectionEvent::Connection(None) => return ConnectionEnd::SessionClosed,
            ConnectionEvent::Drain => {
                flush_before_closing(pump, &mut writer, &mut output_buffer, cx).await;
                return ConnectionEnd::SessionClosed;
            }
            ConnectionEvent::Received(None) => return ConnectionEnd::Lost,
            ConnectionEvent::Received(Some(envelope)) => {
                if pump.incoming_tx.unbounded_send(envelope).is_err() {
                    return ConnectionEnd::SessionClosed;
                }
                continue;
            }
            ConnectionEvent::Outgoing(None) => return ConnectionEnd::SessionClosed,
            ConnectionEvent::Outgoing(Some(envelope)) => {
                write_or_yield(
                    &mut pump.connections,
                    async {
                        write_envelope_frame(&mut writer, &mut output_buffer, envelope).await?;
                        writer.flush().await?;
                        anyhow::Ok(())
                    },
                    cx,
                )
                .await
            }
            ConnectionEvent::Log(None) => continue,
            ConnectionEvent::Log(Some(chunk)) => {
                write_or_yield(
                    &mut pump.connections,
                    async {
                        write_daemon_frame(&mut writer, DaemonFrameKind::Log, &chunk).await?;
                        writer.flush().await?;
                        anyhow::Ok(())
                    },
                    cx,
                )
                .await
            }
        };
        match written {
            WriteOutcome::Written => {}
            WriteOutcome::Replaced(Some(connection)) => {
                return ConnectionEnd::Replaced(connection);
            }
            WriteOutcome::Replaced(None) => return ConnectionEnd::SessionClosed,
            WriteOutcome::Failed(error) => {
                log::warn!(
                    "[{}] failed to write to the proxy: {error:#}",
                    pump.identifier
                );
                return ConnectionEnd::Lost;
            }
        }
    }
}

/// Writes to the proxy unless a newer connection arrives first. A proxy that
/// has stopped reading would otherwise hold the write — and the reconnecting
/// client behind the newer connection with it.
async fn write_or_yield(
    connections: &mut mpsc::UnboundedReceiver<DaemonConnection>,
    write: impl Future<Output = Result<()>>,
    cx: &AsyncApp,
) -> WriteOutcome {
    let write = write
        .with_timeout(STALLED_WRITE_TIMEOUT, cx.background_executor())
        .fuse();
    futures::pin_mut!(write);
    select_biased! {
        connection = connections.next().fuse() => WriteOutcome::Replaced(connection),
        written = write => match written {
            Ok(Ok(())) => WriteOutcome::Written,
            Ok(Err(error)) => WriteOutcome::Failed(error),
            Err(_) => WriteOutcome::Failed(anyhow!(
                "the proxy took nothing for {STALLED_WRITE_TIMEOUT:?}"
            )),
        },
    }
}

/// Puts what the session has already queued on the wire before its
/// connection goes.
async fn flush_before_closing(
    pump: &mut SessionPump,
    writer: &mut WriteHalf<DaemonConnection>,
    buffer: &mut Vec<u8>,
    cx: &AsyncApp,
) {
    let flushed = async {
        while let Some(Some(envelope)) = pump.outgoing_rx.next().now_or_never() {
            write_envelope_frame(writer, buffer, envelope).await?;
        }
        writer.flush().await?;
        anyhow::Ok(())
    }
    .with_timeout(DRAIN_TIMEOUT, cx.background_executor())
    .await;
    match flushed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => log::info!(
            "[{}] could not deliver the last messages: {error:#}",
            pump.identifier
        ),
        Err(_) => log::info!(
            "[{}] gave up delivering the last messages after {DRAIN_TIMEOUT:?}",
            pump.identifier
        ),
    }
}

/// Whether `accept` failed for a reason that passes: running out of file
/// descriptors, or a connection that went away before it was taken. Every
/// session shares this one process's limits now, so the first of these is
/// no longer a reason to stop serving all of them.
fn is_transient_accept_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::ConnectionAborted
            | ErrorKind::WouldBlock
            | ErrorKind::OutOfMemory
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM | libc::ECONNABORTED)
        )
    }
    #[cfg(windows)]
    {
        // WSAEMFILE and WSAENOBUFS.
        matches!(error.raw_os_error(), Some(10024 | 10055))
    }
}

/// Whether the process at the other end of `stream` runs as this user. The
/// socket's directory already keeps other users out; this is the second lock
/// on the same door.
#[cfg(target_os = "linux")]
pub(crate) fn peer_is_this_user(stream: &impl std::os::fd::AsRawFd) -> std::io::Result<bool> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `len` outlive the call, and `len` holds the
    // size of `credentials`, which is what `SO_PEERCRED` fills in.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    Ok(credentials.uid == unsafe { libc::geteuid() })
}

#[cfg(target_os = "macos")]
pub(crate) fn peer_is_this_user(stream: &impl std::os::fd::AsRawFd) -> std::io::Result<bool> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: `uid` and `gid` outlive the call, which only writes to them.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    Ok(uid == unsafe { libc::geteuid() })
}

/// Accepts the proxies' connections for as long as the daemon runs.
pub(crate) fn serve(listener: UnixListener, registry: WeakEntity<SessionRegistry>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut backoff = MIN_ACCEPT_BACKOFF;
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => {
                    backoff = MIN_ACCEPT_BACKOFF;
                    stream
                }
                Err(error) if is_transient_accept_error(&error) => {
                    log::warn!(
                        "could not accept a proxy's connection ({error}); trying again in {backoff:?}"
                    );
                    cx.background_executor().timer(backoff).await;
                    backoff = (backoff * 2).min(MAX_ACCEPT_BACKOFF);
                    continue;
                }
                Err(error) => {
                    // No proxy can reach any session without the listener, so
                    // there is nothing left for this process to do.
                    log::error!("failed to accept a proxy's connection: {error:?}; exiting");
                    cx.update(|cx| {
                        cx.shutdown();
                        cx.quit();
                    });
                    return;
                }
            };

            #[cfg(any(target_os = "linux", target_os = "macos"))]
            match peer_is_this_user(&stream) {
                Ok(true) => {}
                Ok(false) => {
                    log::warn!("refused a connection from a process of another user");
                    continue;
                }
                Err(error) => {
                    log::warn!("refused a connection whose owner could not be read: {error}");
                    continue;
                }
            }

            let registry = registry.clone();
            cx.spawn(async move |cx| {
                if let Err(error) = admit_connection(Box::new(stream), &registry, cx).await {
                    log::warn!("could not take a proxy's connection: {error:#}");
                }
            })
            .detach();
        }
    })
    .detach();
}

/// Reads a proxy's introduction and hands its connection to the session it
/// names.
pub(crate) async fn admit_connection(
    mut connection: DaemonConnection,
    registry: &WeakEntity<SessionRegistry>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let hello = read_json_message::<_, DaemonHello>(&mut connection)
        .with_timeout(HELLO_TIMEOUT, cx.background_executor())
        .await
        .map_err(|_| anyhow!("the proxy did not introduce itself within {HELLO_TIMEOUT:?}"))??;

    if hello.protocol_version != DAEMON_PROTOCOL_VERSION {
        let message = format!(
            "this daemon speaks protocol {DAEMON_PROTOCOL_VERSION}, the proxy {}",
            hello.protocol_version
        );
        write_json_message(
            &mut connection,
            &DaemonHelloReply {
                outcome: DaemonHelloOutcome::Rejected,
                message: Some(message.clone()),
            },
        )
        .await?;
        anyhow::bail!(message);
    }

    let admission = registry.update(cx, |registry, cx| registry.admit(&hello, cx))?;
    let connections = match admission {
        Admission::Accepted(connections) => connections,
        Admission::SessionNotFound => {
            log::info!(
                "[{}] refused a reconnect: this daemon has no such session",
                hello.identifier
            );
            return write_json_message(
                &mut connection,
                &DaemonHelloReply {
                    outcome: DaemonHelloOutcome::SessionNotFound,
                    message: None,
                },
            )
            .await;
        }
    };

    // The reply has to be on the wire before the session writes its first
    // frame, which it may do as soon as it holds the connection.
    write_json_message(
        &mut connection,
        &DaemonHelloReply {
            outcome: DaemonHelloOutcome::Accepted,
            message: None,
        },
    )
    .await?;
    connections.unbounded_send(connection).map_err(|_| {
        anyhow!(
            "[{}] the session closed before it could take its connection",
            hello.identifier
        )
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use futures::{AsyncRead, AsyncWrite};
    use std::{
        collections::VecDeque,
        io,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll, Waker},
    };

    #[derive(Default)]
    struct Pipe {
        bytes: VecDeque<u8>,
        capacity: Option<usize>,
        closed: bool,
        reader: Option<Waker>,
        writer: Option<Waker>,
    }

    /// One end of an in-memory connection, standing in for a socket or a pipe.
    pub(crate) struct MemoryStream {
        incoming: Arc<Mutex<Pipe>>,
        outgoing: Arc<Mutex<Pipe>>,
    }

    pub(crate) fn memory_stream_pair() -> (MemoryStream, MemoryStream) {
        memory_stream_pair_with_capacity(None)
    }

    /// A pair whose first end can have at most `capacity` bytes waiting for the
    /// second end to read. A second end that stops reading then blocks the
    /// first one's writes, as a full socket buffer does.
    pub(crate) fn memory_stream_pair_with_capacity(
        capacity: Option<usize>,
    ) -> (MemoryStream, MemoryStream) {
        let first_to_second = Arc::new(Mutex::new(Pipe {
            capacity,
            ..Pipe::default()
        }));
        let second_to_first = Arc::new(Mutex::new(Pipe::default()));
        (
            MemoryStream {
                incoming: second_to_first.clone(),
                outgoing: first_to_second.clone(),
            },
            MemoryStream {
                incoming: first_to_second,
                outgoing: second_to_first,
            },
        )
    }

    impl MemoryStream {
        fn hang_up(&self) {
            for pipe in [&self.incoming, &self.outgoing] {
                let mut pipe = pipe.lock().expect("pipe lock");
                pipe.closed = true;
                if let Some(reader) = pipe.reader.take() {
                    reader.wake();
                }
                if let Some(writer) = pipe.writer.take() {
                    writer.wake();
                }
            }
        }
    }

    impl Drop for MemoryStream {
        fn drop(&mut self) {
            self.hang_up();
        }
    }

    impl AsyncRead for MemoryStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let mut pipe = self.incoming.lock().expect("pipe lock");
            if !pipe.bytes.is_empty() {
                let len = buffer.len().min(pipe.bytes.len());
                for (slot, byte) in buffer.iter_mut().zip(pipe.bytes.drain(..len)) {
                    *slot = byte;
                }
                if let Some(writer) = pipe.writer.take() {
                    writer.wake();
                }
                return Poll::Ready(Ok(len));
            }
            if pipe.closed {
                return Poll::Ready(Ok(0));
            }
            pipe.reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    impl AsyncWrite for MemoryStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut pipe = self.outgoing.lock().expect("pipe lock");
            if pipe.closed {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            let room = pipe
                .capacity
                .map_or(buffer.len(), |capacity| {
                    capacity.saturating_sub(pipe.bytes.len())
                })
                .min(buffer.len());
            if room == 0 && !buffer.is_empty() {
                pipe.writer = Some(cx.waker().clone());
                return Poll::Pending;
            }
            pipe.bytes.extend(&buffer[..room]);
            if let Some(reader) = pipe.reader.take() {
                reader.wake();
            }
            Poll::Ready(Ok(room))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.hang_up();
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{MemoryStream, memory_stream_pair_with_capacity};
    use super::*;
    use crate::headless_project::HeadlessHostState;
    use extension::ExtensionHostProxy;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use http_client::{AllowlistedHttpClient, BlockedHttpClient, EgressPolicyHandle, HttpClient};
    use language::LanguageRegistry;
    use node_runtime::NodeRuntime;
    use rpc::proto::{
        self, EnvelopedMessage as _, REMOTE_SERVER_PROJECT_ID, jp_enhance::JP_ENHANCE_WIRE_REVISION,
    };
    use serde_json::json;
    use std::{cell::Cell, time::Instant};

    struct TestDaemon {
        registry: Entity<SessionRegistry>,
        host: Entity<HeadlessHost>,
        router: SessionRouter,
        fs: Arc<FakeFs>,
        exits: Rc<Cell<usize>>,
        log_tx: smol::channel::Sender<Vec<u8>>,
    }

    fn start_daemon(http_client: Arc<dyn HttpClient>, cx: &mut TestAppContext) -> TestDaemon {
        cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            HeadlessProject::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        let host = cx.new(|cx| {
            HeadlessHost::new(
                HeadlessHostState {
                    fs: fs.clone(),
                    http_client,
                    node_runtime: NodeRuntime::unavailable(),
                    languages: Arc::new(LanguageRegistry::new(cx.background_executor().clone())),
                    extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                    startup_time: Instant::now(),
                },
                cx,
            )
        });
        let router = SessionRouter::default();
        let (proxy_http_relay, _transport) = cx.update(|cx| {
            ProxyHttpRelay::new_with_session_source(
                {
                    let router = router.clone();
                    Arc::new(move || router.connected_client())
                },
                cx,
            )
        });
        let exits = Rc::new(Cell::new(0));
        let (log_tx, log_rx) = smol::channel::unbounded();
        let registry = cx.new(|cx| {
            SessionRegistry::new(
                SessionRegistryOptions {
                    host: host.clone(),
                    router: router.clone(),
                    proxy_http_relay,
                    is_wsl_interop: false,
                    init_worktree_trust: false,
                    on_idle: Rc::new({
                        let exits = exits.clone();
                        move |_| exits.set(exits.get() + 1)
                    }),
                },
                log_rx,
                cx,
            )
        });
        TestDaemon {
            registry,
            host,
            router,
            fs,
            exits,
            log_tx,
        }
    }

    fn hello(identifier: &str, reconnect: bool) -> DaemonHello {
        DaemonHello {
            protocol_version: DAEMON_PROTOCOL_VERSION,
            identifier: identifier.to_string(),
            reconnect,
            ssh_auth_sock: None,
        }
    }

    /// Introduces a proxy whose connection holds at most `capacity` bytes the
    /// proxy has not read yet.
    async fn introduce(
        daemon: &TestDaemon,
        hello: DaemonHello,
        capacity: Option<usize>,
        cx: &mut TestAppContext,
    ) -> (DaemonHelloReply, MemoryStream) {
        let (daemon_end, mut proxy_end) = memory_stream_pair_with_capacity(capacity);
        let admitting =
            cx.spawn({
                let registry = daemon.registry.downgrade();
                move |mut cx| async move {
                    admit_connection(Box::new(daemon_end), &registry, &mut cx).await
                }
            });
        write_json_message(&mut proxy_end, &hello)
            .await
            .expect("the proxy should be able to introduce itself");
        let reply: DaemonHelloReply = read_json_message(&mut proxy_end)
            .await
            .expect("the daemon should answer the introduction");
        // A rejected introduction is an error on the daemon's side too; what
        // the proxy was told is what these tests look at.
        admitting.await.ok();
        (reply, proxy_end)
    }

    async fn connect(
        daemon: &TestDaemon,
        identifier: &str,
        reconnect: bool,
        cx: &mut TestAppContext,
    ) -> (DaemonHelloOutcome, MemoryStream) {
        let (reply, mut stream) = introduce(daemon, hello(identifier, reconnect), None, cx).await;
        // What a client asks first on a fresh session; a resumed one was
        // checked when it started.
        if reply.outcome == DaemonHelloOutcome::Accepted && !reconnect {
            let answer = check_wire_revision(&mut stream, JP_ENHANCE_WIRE_REVISION).await;
            assert_eq!(answer, Some(JP_ENHANCE_WIRE_REVISION));
        }
        (reply.outcome, stream)
    }

    const WIRE_REVISION_CHECK_ID: u32 = 9_999;

    async fn check_wire_revision<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        revision: u32,
    ) -> Option<u32> {
        let answer = request(
            stream,
            proto::CheckWireRevision {
                revision,
                display_language: None,
            }
            .into_envelope(WIRE_REVISION_CHECK_ID, None, None),
        )
        .await;
        proto::CheckWireRevisionResponse::from_envelope(answer).map(|answer| answer.revision)
    }

    async fn send<S: AsyncWrite + Unpin>(stream: &mut S, envelope: Envelope) {
        let mut buffer = Vec::new();
        write_envelope_frame(stream, &mut buffer, envelope)
            .await
            .expect("the connection should take the message");
        stream.flush().await.expect("flushing the connection");
    }

    /// Sends `envelope` and waits for its answer, answering on the way the
    /// requests a client answers (the server asks the client for worktree ids).
    async fn request<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        envelope: Envelope,
    ) -> Envelope {
        let message_id = envelope.id;
        send(stream, envelope).await;
        let mut buffer = Vec::new();
        let mut next_id = 1000;
        loop {
            let kind = read_daemon_frame(stream, &mut buffer)
                .await
                .expect("the session should answer");
            if kind != DaemonFrameKind::Envelope {
                continue;
            }
            let envelope =
                decode_envelope(&buffer).expect("the session should send valid envelopes");
            if envelope.responding_to == Some(message_id) {
                return envelope;
            }
            if proto::AllocateWorktreeId::from_envelope(envelope.clone()).is_some() {
                send(
                    stream,
                    proto::AllocateWorktreeIdResponse { worktree_id: 1 }.into_envelope(
                        next_id,
                        Some(envelope.id),
                        None,
                    ),
                )
                .await;
                next_id += 1;
            }
        }
    }

    async fn ping<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, message_id: u32) {
        request(stream, proto::Ping {}.into_envelope(message_id, None, None)).await;
    }

    /// Reads until the daemon closes the connection, returning the ids of the
    /// messages it answered on the way.
    async fn read_until_closed(stream: &mut MemoryStream) -> Vec<u32> {
        let mut answered = Vec::new();
        let mut buffer = Vec::new();
        while let Ok(kind) = read_daemon_frame(stream, &mut buffer).await {
            if kind == DaemonFrameKind::Envelope
                && let Some(responding_to) = decode_envelope(&buffer)
                    .ok()
                    .and_then(|envelope| envelope.responding_to)
            {
                answered.push(responding_to);
            }
        }
        answered
    }

    fn session_count(daemon: &TestDaemon, cx: &mut TestAppContext) -> usize {
        daemon
            .registry
            .read_with(cx, |registry, _| registry.session_count())
    }

    #[gpui::test]
    async fn test_closing_one_session_leaves_the_others_running(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let (outcome, mut first) = connect(&daemon, "client.workspace-1", false, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        let (outcome, mut second) = connect(&daemon, "client.workspace-2", false, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        ping(&mut first, 1).await;
        ping(&mut second, 1).await;
        assert_eq!(session_count(&daemon, cx), 2);

        send(
            &mut first,
            proto::ShutdownRemoteServer {}.into_envelope(2, None, None),
        )
        .await;
        let answered = read_until_closed(&mut first).await;
        assert!(
            answered.contains(&2),
            "the shutdown should be acknowledged before the connection goes"
        );
        cx.run_until_parked();

        assert_eq!(
            session_count(&daemon, cx),
            1,
            "closing a project must end its session only"
        );
        ping(&mut second, 3).await;
        assert_eq!(daemon.exits.get(), 0, "the daemon still has a session");
    }

    #[gpui::test]
    async fn test_closing_a_session_releases_what_it_held(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        daemon
            .fs
            .insert_tree(
                util::path!("/code"),
                json!({ "project1": { "a.txt": "a" } }),
            )
            .await;
        let (_, mut connection) = connect(&daemon, "client.workspace-1", false, cx).await;
        let added = request(
            &mut connection,
            proto::AddWorktree {
                project_id: REMOTE_SERVER_PROJECT_ID,
                path: util::path!("/code/project1").to_string(),
                visible: true,
                code_runs_without_ide_decision: true,
            }
            .into_envelope(10, None, None),
        )
        .await;
        assert!(proto::AddWorktreeResponse::from_envelope(added).is_some());
        cx.run_until_parked();

        let (project, worktree_store, worktree, buffer_store, lsp_store, dap_store) =
            daemon.registry.read_with(cx, |registry, cx| {
                let project = registry
                    .project("client.workspace-1")
                    .expect("the session should exist");
                let headless = project.read(cx);
                let worktree = headless
                    .worktree_store
                    .read(cx)
                    .worktrees()
                    .next()
                    .expect("the worktree should have been added");
                (
                    project.downgrade(),
                    headless.worktree_store.downgrade(),
                    worktree.downgrade(),
                    headless.buffer_store.downgrade(),
                    headless.lsp_store.downgrade(),
                    headless.dap_store.downgrade(),
                )
            });

        send(
            &mut connection,
            proto::ShutdownRemoteServer {}.into_envelope(11, None, None),
        )
        .await;
        read_until_closed(&mut connection).await;
        cx.run_until_parked();
        // The project's search task holds the buffer store, and a task dropped
        // outside an update leaves what it held to the next effect cycle.
        cx.update(|_| {});

        // `assert_released` rather than `upgrade().is_none()`: an entity whose
        // last handle is gone no longer upgrades, yet keeps everything it
        // holds until an update ends and GPUI releases it.
        project.assert_released();
        worktree.assert_released();
        worktree_store.assert_released();
        buffer_store.assert_released();
        lsp_store.assert_released();
        dap_store.assert_released();
        assert_eq!(
            daemon
                .host
                .read_with(cx, |host, cx| host.live_lsp_stores(cx).len()),
            0
        );
    }

    /// zed-jp-enhance: a client of another wire revision reads and writes the
    /// fork's fields differently (`AddWorktree`'s say over the worktree's
    /// code, `Entry.is_private`), so a session serves nothing but pings until
    /// the client has shown this build's revision.
    #[gpui::test]
    async fn test_a_session_serves_nothing_until_the_client_shows_its_wire_revision(
        cx: &mut TestAppContext,
    ) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        daemon
            .fs
            .insert_tree(
                util::path!("/code"),
                json!({ "project1": { "a.txt": "a" } }),
            )
            .await;
        let (reply, mut connection) =
            introduce(&daemon, hello("client.workspace-1", false), None, cx).await;
        assert_eq!(reply.outcome, DaemonHelloOutcome::Accepted);
        let add_worktree = |message_id| {
            proto::AddWorktree {
                project_id: REMOTE_SERVER_PROJECT_ID,
                path: util::path!("/code/project1").to_string(),
                visible: true,
                code_runs_without_ide_decision: false,
            }
            .into_envelope(message_id, None, None)
        };
        let worktree_count = |cx: &mut TestAppContext| {
            daemon.registry.read_with(cx, |registry, cx| {
                registry
                    .project("client.workspace-1")
                    .expect("the session should exist")
                    .read(cx)
                    .worktree_store
                    .read(cx)
                    .worktrees()
                    .count()
            })
        };

        ping(&mut connection, 1).await;
        let refused = request(&mut connection, add_worktree(2)).await;
        assert!(
            matches!(refused.payload, Some(proto::envelope::Payload::Error(_))),
            "{refused:?}"
        );

        // A client of an older revision is told this one's, and still refused.
        let answer = check_wire_revision(&mut connection, JP_ENHANCE_WIRE_REVISION - 1).await;
        assert_eq!(answer, Some(JP_ENHANCE_WIRE_REVISION));
        let refused = request(&mut connection, add_worktree(3)).await;
        assert!(
            matches!(refused.payload, Some(proto::envelope::Payload::Error(_))),
            "{refused:?}"
        );
        cx.run_until_parked();
        assert_eq!(worktree_count(cx), 0, "no refused message may take effect");

        let answer = check_wire_revision(&mut connection, JP_ENHANCE_WIRE_REVISION).await;
        assert_eq!(answer, Some(JP_ENHANCE_WIRE_REVISION));
        let added = request(&mut connection, add_worktree(4)).await;
        assert!(
            proto::AddWorktreeResponse::from_envelope(added.clone()).is_some(),
            "{added:?}"
        );
        cx.run_until_parked();
        assert_eq!(worktree_count(cx), 1);

        // A resumed connection is the same client, already checked.
        let (outcome, mut resumed) = connect(&daemon, "client.workspace-1", true, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        read_until_closed(&mut connection).await;
        ping(&mut resumed, 5).await;
    }

    #[gpui::test]
    async fn test_a_reconnect_resumes_its_own_session_only(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);

        let (outcome, _) = connect(&daemon, "client.workspace-1", true, cx).await;
        assert_eq!(
            outcome,
            DaemonHelloOutcome::SessionNotFound,
            "there is nothing to resume before the session exists"
        );

        let (_, mut stale) = connect(&daemon, "client.workspace-1", false, cx).await;
        let (_, mut other) = connect(&daemon, "client.workspace-2", false, cx).await;
        ping(&mut stale, 1).await;

        let (outcome, mut resumed) = connect(&daemon, "client.workspace-1", true, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        read_until_closed(&mut stale).await;
        ping(&mut resumed, 2).await;
        ping(&mut other, 1).await;
        assert_eq!(session_count(&daemon, cx), 2);
    }

    #[gpui::test]
    async fn test_a_proxy_that_stopped_reading_does_not_hold_up_its_replacement(
        cx: &mut TestAppContext,
    ) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let (reply, mut stuck) =
            introduce(&daemon, hello("client.workspace-1", false), Some(96), cx).await;
        assert_eq!(reply.outcome, DaemonHelloOutcome::Accepted);
        // Never read: the answers pile up until the daemon's writes block.
        for message_id in 1..=30 {
            send(
                &mut stuck,
                proto::Ping {}.into_envelope(message_id, None, None),
            )
            .await;
        }
        cx.run_until_parked();

        let (outcome, mut fresh) = connect(&daemon, "client.workspace-1", true, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        ping(&mut fresh, 100).await;
    }

    #[gpui::test]
    async fn test_a_fresh_connection_starts_its_session_over(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let (_, mut old) = connect(&daemon, "client.setup-1", false, cx).await;
        ping(&mut old, 1).await;
        let old_project = daemon
            .registry
            .read_with(cx, |registry, _| registry.project("client.setup-1"))
            .expect("the session should exist")
            .downgrade();

        let (outcome, mut new) = connect(&daemon, "client.setup-1", false, cx).await;
        assert_eq!(outcome, DaemonHelloOutcome::Accepted);
        read_until_closed(&mut old).await;
        ping(&mut new, 1).await;
        cx.run_until_parked();

        assert_eq!(session_count(&daemon, cx), 1);
        assert!(
            old_project.upgrade().is_none(),
            "the stale session's project should have been released"
        );
    }

    #[gpui::test]
    async fn test_an_unattended_session_idles_out_and_then_the_daemon_exits(
        cx: &mut TestAppContext,
    ) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let (_, mut connection) = connect(&daemon, "client.workspace-1", false, cx).await;
        ping(&mut connection, 1).await;
        drop(connection);
        cx.run_until_parked();

        cx.executor()
            .advance_clock(SESSION_IDLE_TIMEOUT - Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            session_count(&daemon, cx),
            1,
            "a session waits for its client to come back"
        );

        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(session_count(&daemon, cx), 0);
        assert_eq!(daemon.exits.get(), 0, "the daemon lingers for a while");

        cx.executor().advance_clock(EMPTY_DAEMON_LINGER);
        cx.run_until_parked();
        assert_eq!(daemon.exits.get(), 1);
    }

    #[gpui::test]
    async fn test_a_new_session_keeps_the_daemon_from_exiting(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        cx.executor()
            .advance_clock(EMPTY_DAEMON_LINGER - Duration::from_secs(1));
        let (_, mut connection) = connect(&daemon, "client.workspace-1", false, cx).await;
        ping(&mut connection, 1).await;
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        assert_eq!(daemon.exits.get(), 0);
    }

    #[gpui::test]
    async fn test_sessions_share_the_host(cx: &mut TestAppContext) {
        // An allowlisted client is what gives the host a child-process proxy
        // supervisor; no allowlist is pushed, so none is started.
        let daemon = start_daemon(
            Arc::new(AllowlistedHttpClient::new(
                Arc::new(BlockedHttpClient),
                EgressPolicyHandle::default(),
            )),
            cx,
        );
        let (_, mut first) = connect(&daemon, "client.workspace-1", false, cx).await;
        let (_, mut second) = connect(&daemon, "client.workspace-2", false, cx).await;
        ping(&mut first, 1).await;
        ping(&mut second, 1).await;

        let (shares_languages, shares_supervisor) =
            daemon.registry.read_with(cx, |registry, cx| {
                let first = registry
                    .project("client.workspace-1")
                    .expect("the first session should exist");
                let second = registry
                    .project("client.workspace-2")
                    .expect("the second session should exist");
                let (first, second) = (first.read(cx), second.read(cx));
                let shares_supervisor = match (
                    first.child_process_proxy.as_ref(),
                    second.child_process_proxy.as_ref(),
                ) {
                    (Some(first), Some(second)) => Rc::ptr_eq(first, second),
                    _ => false,
                };
                (
                    Arc::ptr_eq(&first.languages, &second.languages),
                    shares_supervisor,
                )
            });
        assert!(
            shares_languages,
            "language server ids come from the registry, so it has to be one per process"
        );
        assert!(shares_supervisor);
        assert_eq!(
            daemon
                .host
                .read_with(cx, |host, cx| host.live_lsp_stores(cx).len()),
            2,
            "extensions have to reach every project's language servers"
        );

        send(
            &mut first,
            proto::ShutdownRemoteServer {}.into_envelope(2, None, None),
        )
        .await;
        read_until_closed(&mut first).await;
        cx.run_until_parked();

        assert_eq!(
            daemon
                .host
                .read_with(cx, |host, cx| host.live_lsp_stores(cx).len()),
            1
        );
        assert!(
            daemon
                .host
                .read_with(cx, |host, _| host.has_child_process_proxy_supervisor()),
            "the child-process proxy belongs to the process, not to the project that closed"
        );
        ping(&mut second, 3).await;
    }

    #[gpui::test]
    async fn test_the_log_goes_to_the_most_recently_connected_session(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let (_, mut first) = connect(&daemon, "client.workspace-1", false, cx).await;
        let (_, mut second) = connect(&daemon, "client.workspace-2", false, cx).await;
        ping(&mut first, 1).await;
        ping(&mut second, 1).await;

        daemon
            .log_tx
            .send(b"{\"message\":\"hello\"}\n".to_vec())
            .await
            .expect("the log channel should be open");
        cx.run_until_parked();

        let mut buffer = Vec::new();
        let mut second_got_it = false;
        while let Some(frame) = read_daemon_frame(&mut second, &mut buffer).now_or_never() {
            if frame.expect("reading the second connection") == DaemonFrameKind::Log {
                assert_eq!(buffer, b"{\"message\":\"hello\"}\n");
                second_got_it = true;
            }
        }
        assert!(second_got_it);
        while let Some(frame) = read_daemon_frame(&mut first, &mut buffer).now_or_never() {
            assert_ne!(
                frame.expect("reading the first connection"),
                DaemonFrameKind::Log,
                "one copy of the log is enough for one IDE"
            );
        }
    }

    #[gpui::test]
    async fn test_the_router_prefers_the_newest_connection(cx: &mut TestAppContext) {
        let router = SessionRouter::default();
        let mut log_receivers = Vec::new();
        for id in 0..2 {
            let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
            let (outgoing_tx, _outgoing_rx) = mpsc::unbounded::<Envelope>();
            drop(incoming_tx);
            let client = cx.update(|cx| {
                RemoteClient::proto_client_from_channels(
                    incoming_rx,
                    outgoing_tx,
                    cx,
                    "test",
                    false,
                )
            });
            let (log_tx, log_rx) = mpsc::unbounded();
            router.add(SessionId(id), client, log_tx);
            log_receivers.push(log_rx);
        }
        let log_goes_to =
            |router: &SessionRouter, log_receivers: &mut Vec<mpsc::UnboundedReceiver<Vec<u8>>>| {
                if let Some(sink) = router.log_sink() {
                    sink.unbounded_send(Vec::new()).ok();
                }
                log_receivers
                    .iter_mut()
                    .position(|receiver| receiver.next().now_or_never().flatten().is_some())
            };

        assert!(
            router.preferred_client().is_some(),
            "with nothing connected, a session that may reconnect still beats none"
        );
        assert!(
            router.connected_client().is_none(),
            "the HTTP relay must not pick a session nobody is connected to"
        );
        assert_eq!(log_goes_to(&router, &mut log_receivers), None);

        router.set_connected(SessionId(1), true);
        router.set_connected(SessionId(0), true);
        assert!(router.connected_client().is_some());
        assert_eq!(log_goes_to(&router, &mut log_receivers), Some(0));

        router.set_connected(SessionId(0), false);
        assert_eq!(log_goes_to(&router, &mut log_receivers), Some(1));

        router.remove(SessionId(1));
        assert_eq!(log_goes_to(&router, &mut log_receivers), None);
        assert_eq!(router.all_clients().len(), 1);
    }

    #[gpui::test]
    async fn test_the_ssh_agent_of_the_newest_connection_is_followed(cx: &mut TestAppContext) {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let first_agent = temp_dir.path().join("agent.1");
        let second_agent = temp_dir.path().join("agent.2");
        std::fs::write(&first_agent, "").expect("create the first agent socket stand-in");
        std::fs::write(&second_agent, "").expect("create the second agent socket stand-in");
        let path_string = |path: &Path| path.to_string_lossy().into_owned();

        let router = SessionRouter::default();
        for id in 0..2 {
            let (incoming_tx, incoming_rx) = mpsc::unbounded::<Envelope>();
            let (outgoing_tx, _outgoing_rx) = mpsc::unbounded::<Envelope>();
            drop(incoming_tx);
            let client = cx.update(|cx| {
                RemoteClient::proto_client_from_channels(
                    incoming_rx,
                    outgoing_tx,
                    cx,
                    "test",
                    false,
                )
            });
            router.add(SessionId(id), client, mpsc::unbounded().0);
        }
        router.set_ssh_auth_sock(SessionId(0), Some(path_string(&first_agent)));
        router.set_ssh_auth_sock(SessionId(1), Some(path_string(&second_agent)));
        // Marked connected without `set_connected`, which would point this
        // test process's own environment at the stand-ins.
        {
            let mut state = router.state();
            state.sessions[0].connection = Some(1);
            state.sessions[1].connection = Some(2);
        }
        assert_eq!(
            router.ssh_agent_to_follow(),
            Some(path_string(&second_agent))
        );

        std::fs::remove_file(&second_agent).expect("remove the second agent socket stand-in");
        assert_eq!(
            router.ssh_agent_to_follow(),
            Some(path_string(&first_agent)),
            "an agent that has gone away is no use to follow"
        );
    }

    #[gpui::test]
    async fn test_a_proxy_of_another_protocol_is_turned_away(cx: &mut TestAppContext) {
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let mut stale_hello = hello("client.workspace-1", false);
        stale_hello.protocol_version = DAEMON_PROTOCOL_VERSION + 1;
        let (reply, _) = introduce(&daemon, stale_hello, None, cx).await;
        assert_eq!(reply.outcome, DaemonHelloOutcome::Rejected);
        assert!(reply.message.is_some());
        assert_eq!(session_count(&daemon, cx), 0);
        assert!(daemon.router.all_clients().is_empty());
    }

    #[test]
    fn running_out_of_file_descriptors_does_not_stop_the_daemon() {
        #[cfg(unix)]
        let too_many_open_files = std::io::Error::from_raw_os_error(libc::EMFILE);
        #[cfg(windows)]
        let too_many_open_files = std::io::Error::from_raw_os_error(10024);
        assert!(is_transient_accept_error(&too_many_open_files));
        assert!(is_transient_accept_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_transient_accept_error(&std::io::Error::from(
            std::io::ErrorKind::InvalidInput
        )));
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_serve_admits_a_proxy_over_a_real_socket(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let daemon = start_daemon(Arc::new(BlockedHttpClient), cx);
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let socket = temp_dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind the daemon socket");
        cx.update(|cx| serve(listener, daemon.registry.downgrade(), cx));

        let mut stream = net::async_net::UnixStream::connect(&socket)
            .await
            .expect("connect to the daemon");
        write_json_message(&mut stream, &hello("client.workspace-1", false))
            .await
            .expect("introduce the proxy");
        let reply: DaemonHelloReply = read_json_message(&mut stream)
            .await
            .expect("read the daemon's answer");
        assert_eq!(reply.outcome, DaemonHelloOutcome::Accepted);
        ping(&mut stream, 1).await;
    }
}
