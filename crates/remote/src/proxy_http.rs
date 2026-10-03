//! The wire half of ADR-F27 (`.forgekairos-docs/DESIGN.md` §4.27): a remote host
//! that cannot reach the internet hands its outgoing HTTP(S) to the connected
//! IDE, which runs it through its own client — network allowlist (ADR-F03),
//! download consent (ADR-F22) and proxy included.
//!
//! Both ends live here because they are one protocol and the encoding has to
//! agree; the crate is already linked into both binaries.
//!
//! * On the **IDE**, [`handle_proxy_http_request`] runs the request and streams
//!   the answer back in `ProxyHttpResponse` frames.
//! * On the **remote host**, [`ProxyHttpRelay`] tracks the requests in flight
//!   and hands `http_client`'s [`RemoteProxyHttpClient`] a transport that
//!   speaks to the IDE.
//!
//! [`RemoteProxyHttpClient`]: http_client::RemoteProxyHttpClient

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use bytes::Bytes;
use collections::HashMap;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{AsyncReadExt as _, FutureExt as _, SinkExt as _, StreamExt as _};
use gpui::{App, AppContext as _, AsyncApp, BackgroundExecutor, Entity, Global, WeakEntity};
use http_client::{
    AsyncBody, HttpClient, HttpRequestExt as _, Method, PROXY_HTTP_CHUNK_SIZE, ProxyHttpFrame,
    ProxyHttpFrames, ProxyHttpRelayError, ProxyHttpRelayRequest, ProxyHttpTransport,
    RedirectPolicy, Request, Response,
};
use parking_lot::Mutex;
use rpc::AnyProtoClient;
use rpc::proto::{self, TypedEnvelope};

use crate::remote_client::RemoteClient;

/// How many frames may sit between the IDE and whoever is reading the body.
///
/// This is the relay's backpressure: the IDE waits for the acknowledgement of
/// each frame it sends, so a reader that has stopped consuming stops the
/// download within this many chunks instead of letting a 50 MB archive pile up
/// in memory on either side.
const FRAME_QUEUE_DEPTH: usize = 2;

/// How long a relayed request waits for its next frame before it gives up.
///
/// The RPC channel has no request timeout and the remote server survives a
/// disconnect (it waits for the client to come back), so without this a
/// download interrupted by a client that went away would never finish or fail
/// — and the caller would sit on it forever. Generous, because the first frame
/// only arrives once the IDE has response headers from the origin server.
const FRAME_STALL_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a relayed request waits for the IDE to take it. The IDE answers
/// as soon as it has, so this only runs out on a connection that has died
/// without anyone noticing yet.
const RELAY_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

// -------------------------------------------------------------------------
// Encoding, shared by both ends.
// -------------------------------------------------------------------------

pub fn relay_error_to_proto(error: ProxyHttpRelayError) -> proto::ProxyHttpError {
    let kind = match error {
        ProxyHttpRelayError::EgressDenied { host, url } => {
            proto::proxy_http_error::Kind::EgressDenied(proto::ProxyHttpEgressDenied { host, url })
        }
        ProxyHttpRelayError::Network { message } => {
            proto::proxy_http_error::Kind::Network(proto::ProxyHttpNetworkError { message })
        }
        ProxyHttpRelayError::Other { message } => {
            proto::proxy_http_error::Kind::Other(proto::ProxyHttpOtherError { message })
        }
    };
    proto::ProxyHttpError { kind: Some(kind) }
}

pub fn relay_error_from_proto(error: proto::ProxyHttpError) -> ProxyHttpRelayError {
    match error.kind {
        Some(proto::proxy_http_error::Kind::EgressDenied(denied)) => {
            ProxyHttpRelayError::EgressDenied {
                host: denied.host,
                url: denied.url,
            }
        }
        Some(proto::proxy_http_error::Kind::Network(error)) => ProxyHttpRelayError::Network {
            message: error.message,
        },
        Some(proto::proxy_http_error::Kind::Other(error)) => ProxyHttpRelayError::Other {
            message: error.message,
        },
        // A peer that sends a kind this build does not know still means "this
        // failed", so report it as one rather than as a successful empty body.
        None => ProxyHttpRelayError::Other {
            message: "the peer reported a failure of an unknown kind".to_string(),
        },
    }
}

fn encode_headers(headers: Vec<(String, String)>) -> Vec<proto::ProxyHttpHeader> {
    headers
        .into_iter()
        .map(|(name, value)| proto::ProxyHttpHeader { name, value })
        .collect()
}

fn decode_headers(headers: Vec<proto::ProxyHttpHeader>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .map(|header| (header.name, header.value))
        .collect()
}

// -------------------------------------------------------------------------
// IDE side: run the request the remote host asked for.
// -------------------------------------------------------------------------

/// Runs a remote host's HTTP request on this machine and streams the response
/// back to it.
///
/// Answers `Ack` as soon as the request has been taken, not when the download
/// finishes: the remote host waits on this acknowledgement to learn that the
/// relay exists at all, and it must not be held for the length of a transfer.
pub(crate) async fn handle_proxy_http_request(
    this: Entity<RemoteClient>,
    envelope: TypedEnvelope<proto::ProxyHttpRequest>,
    cx: AsyncApp,
) -> Result<proto::Ack> {
    // ADR-F38: the far side has already judged this request against the
    // allowlist it was pushed (ADR-F11) before asking for the relay, and what
    // it relays is its IDE tooling — language servers, node, extensions. Its
    // AI child processes go through the proxy (ADR-F14), not through here.
    let http_client = cx.update(|cx| http_client::ide_lane(cx.http_client()));
    let this = this.downgrade();
    let payload = envelope.payload;
    let request_id = payload.request_id;
    let url = payload.url.clone();
    cx.spawn(async move |cx| {
        if let Err(error) = relay_request(&this, payload, http_client, cx).await {
            // The far end has either been told already (through an error
            // frame) or has gone away, so there is nobody left to report to.
            log::debug!("relayed request {request_id} for {url} ended early: {error:#}");
        }
    })
    .detach();
    Ok(proto::Ack {})
}

async fn relay_request(
    this: &WeakEntity<RemoteClient>,
    payload: proto::ProxyHttpRequest,
    http_client: Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let request_id = payload.request_id;
    let response = match perform_request(payload, http_client).await {
        Ok(response) => response,
        Err(error) => {
            let error = ProxyHttpRelayError::from_anyhow(&error);
            log::info!("relayed request {request_id} failed on this machine: {error}");
            return send_frame(
                this,
                proto::ProxyHttpResponse {
                    request_id,
                    is_last: true,
                    error: Some(relay_error_to_proto(error)),
                    ..Default::default()
                },
                cx,
            )
            .await;
        }
    };

    let status = u32::from(response.status().as_u16());
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_string(), value.to_string()))
        })
        .collect::<Vec<_>>();
    let mut head = Some((status, headers));
    let mut body = response.into_body();
    let mut buffer = vec![0u8; PROXY_HTTP_CHUNK_SIZE];

    loop {
        let (chunk, is_last, error) = match read_chunk(&mut body, &mut buffer).await {
            Ok(0) => (Vec::new(), true, None),
            Ok(filled) => (buffer[..filled].to_vec(), false, None),
            Err(error) => (
                Vec::new(),
                true,
                Some(relay_error_to_proto(ProxyHttpRelayError::Network {
                    message: format!("reading the response body: {error}"),
                })),
            ),
        };
        // Only the first frame describes the response; the rest are body.
        let (status, headers) = head.take().unwrap_or_default();
        send_frame(
            this,
            proto::ProxyHttpResponse {
                request_id,
                status,
                headers: encode_headers(headers),
                chunk,
                is_last,
                error,
            },
            cx,
        )
        .await?;
        if is_last {
            return Ok(());
        }
    }
}

async fn perform_request(
    payload: proto::ProxyHttpRequest,
    http_client: Arc<dyn HttpClient>,
) -> Result<Response<AsyncBody>> {
    let method = Method::from_bytes(payload.method.as_bytes())
        .with_context(|| format!("unsupported HTTP method `{}`", payload.method))?;
    let mut request = Request::builder()
        .method(method)
        .uri(&payload.url)
        .follow_redirects(if payload.follow_redirects {
            RedirectPolicy::FollowAll
        } else {
            RedirectPolicy::NoFollow
        })
        // A zero deadline would make `HttpRequestExt::timeout` assert, and it
        // is not a deadline the caller can have meant.
        .when_some(
            payload.timeout_millis.filter(|millis| *millis > 0),
            |request, millis| request.timeout(Duration::from_millis(millis)),
        );
    for header in payload.headers {
        request = request.header(header.name, header.value);
    }
    let body = if payload.body.is_empty() {
        AsyncBody::empty()
    } else {
        AsyncBody::from(payload.body)
    };
    let request = request
        .body(body)
        .with_context(|| format!("building the relayed request for {}", payload.url))?;
    http_client.send(request).await
}

/// Fills `buffer` from `body`, returning how much was read. `0` means the body
/// ended.
async fn read_chunk(body: &mut AsyncBody, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = body.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// Sends one response frame and waits for the remote host to take it.
///
/// Waiting is what paces the transfer: the remote host acknowledges a frame
/// once it has room for it, so a reader over there that has stopped consuming
/// — or has dropped the download altogether — stops this loop instead of
/// letting it push a whole archive into the connection's replay buffer.
async fn send_frame(
    this: &WeakEntity<RemoteClient>,
    frame: proto::ProxyHttpResponse,
    cx: &mut AsyncApp,
) -> Result<()> {
    let request = this.read_with(cx, |this, _| this.proto_client().request(frame))?;
    request
        .await
        .context("the remote host stopped taking this response")?;
    Ok(())
}

// -------------------------------------------------------------------------
// Remote host side: ask the IDE, and reassemble what comes back.
// -------------------------------------------------------------------------

struct PendingRequest {
    frames: mpsc::Sender<Result<ProxyHttpFrame>>,
    /// The response head is carried by the first frame only, so the rest are
    /// body even though the message shape can hold a status line.
    head_sent: bool,
}

#[derive(Default)]
struct RelayState {
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
}

impl RelayState {
    fn register(&self, request_id: u64, frames: mpsc::Sender<Result<ProxyHttpFrame>>) {
        self.pending.lock().insert(
            request_id,
            PendingRequest {
                frames,
                head_sent: false,
            },
        );
    }

    /// The sink for `request_id`, and whether its head has already gone out.
    /// Marks the head as sent, so the caller must use what it gets back.
    fn frame_sink(&self, request_id: u64) -> Option<(mpsc::Sender<Result<ProxyHttpFrame>>, bool)> {
        let mut pending = self.pending.lock();
        let request = pending.get_mut(&request_id)?;
        let head_sent = std::mem::replace(&mut request.head_sent, true);
        Some((request.frames.clone(), head_sent))
    }

    fn remove(&self, request_id: u64) {
        self.pending.lock().remove(&request_id);
    }
}

/// Deregisters a request once nothing is listening for its frames any more, so
/// that the IDE's next frame for it is refused and the transfer is cut short.
struct PendingGuard {
    state: Arc<RelayState>,
    request_id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.remove(self.request_id);
    }
}

/// The remote host's end of the relay. Owns the requests in flight and handles
/// the response frames the IDE sends back.
pub struct ProxyHttpRelay {
    state: Arc<RelayState>,
}

struct GlobalProxyHttpRelay(#[allow(dead_code)] Entity<ProxyHttpRelay>);

impl Global for GlobalProxyHttpRelay {}

/// Picks the connection a relayed request goes out on. Called per request,
/// because on a host that serves several sessions (ADR-F45) the one to use is
/// whichever is connected when the request is made.
pub type ProxyHttpSessionSource = Arc<dyn Fn() -> Option<AnyProtoClient> + Send + Sync>;

impl ProxyHttpRelay {
    /// Wires the relay onto `session` and returns the transport that
    /// `http_client::RemoteProxyHttpClient` sends through.
    pub fn new(session: &AnyProtoClient, cx: &mut App) -> Arc<dyn ProxyHttpTransport> {
        let (relay, transport) = Self::new_with_session_source(
            {
                let session = session.clone();
                Arc::new(move || Some(session.clone()))
            },
            cx,
        );
        Self::attach(&relay, session);
        transport
    }

    /// Builds a relay whose requests go out on whatever `sessions` picks, for a
    /// remote host that serves several IDE sessions from one process
    /// (ADR-F45). Each of those sessions has to be [`attach`]ed as well, so
    /// that the IDE's response frames reach the relay whichever connection
    /// they come back on.
    ///
    /// The entity the response handlers are bound to is kept in a global: it
    /// has no owner otherwise, and the handlers must outlive every request.
    ///
    /// [`attach`]: Self::attach
    pub fn new_with_session_source(
        sessions: ProxyHttpSessionSource,
        cx: &mut App,
    ) -> (Entity<Self>, Arc<dyn ProxyHttpTransport>) {
        let state = Arc::new(RelayState::default());
        let relay = cx.new(|_| ProxyHttpRelay {
            state: state.clone(),
        });
        cx.set_global(GlobalProxyHttpRelay(relay.clone()));
        let transport = Arc::new(ProxyHttpRelayTransport {
            sessions,
            state,
            executor: cx.background_executor().clone(),
        });
        (relay, transport)
    }

    /// Lets `session` deliver the IDE's response frames to `relay`.
    pub fn attach(relay: &Entity<Self>, session: &AnyProtoClient) {
        session.add_request_handler(relay.downgrade(), Self::handle_proxy_http_response);
    }

    async fn handle_proxy_http_response(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ProxyHttpResponse>,
        cx: AsyncApp,
    ) -> Result<proto::Ack> {
        let state = this.read_with(&cx, |this, _| this.state.clone());
        let payload = envelope.payload;
        let request_id = payload.request_id;

        let Some((mut frames, head_sent)) = state.frame_sink(request_id) else {
            // Nothing is reading this any more — the body was dropped, or the
            // request already ended. Failing tells the IDE to stop sending.
            anyhow::bail!("relayed HTTP request {request_id} is no longer in flight");
        };

        let terminal = payload.is_last || payload.error.is_some();
        let mut outgoing = Vec::new();
        if let Some(error) = payload.error {
            outgoing.push(Err(relay_error_from_proto(error).into_anyhow()));
        } else {
            if !head_sent {
                match u16::try_from(payload.status) {
                    Ok(status) => outgoing.push(Ok(ProxyHttpFrame::Head {
                        status,
                        headers: decode_headers(payload.headers),
                    })),
                    Err(_) => outgoing.push(Err(ProxyHttpRelayError::Other {
                        message: format!(
                            "the IDE reported an out-of-range HTTP status {}",
                            payload.status
                        ),
                    }
                    .into_anyhow())),
                }
            }
            if !payload.chunk.is_empty() {
                outgoing.push(Ok(ProxyHttpFrame::Chunk(Bytes::from(payload.chunk))));
            }
        }

        for frame in outgoing {
            if frames.send(frame).await.is_err() {
                state.remove(request_id);
                anyhow::bail!("relayed HTTP request {request_id} was dropped on this host");
            }
        }
        if terminal {
            state.remove(request_id);
        }
        Ok(proto::Ack {})
    }
}

struct ProxyHttpRelayTransport {
    sessions: ProxyHttpSessionSource,
    state: Arc<RelayState>,
    executor: BackgroundExecutor,
}

impl ProxyHttpTransport for ProxyHttpRelayTransport {
    fn send(&self, request: ProxyHttpRelayRequest) -> BoxFuture<'static, Result<ProxyHttpFrames>> {
        let Some(session) = (self.sessions)() else {
            let url = request.url;
            return futures::future::ready(Err(ProxyHttpRelayError::Network {
                message: format!("no IDE is connected to this host to relay {url} through"),
            }
            .into_anyhow()))
            .boxed();
        };
        let request_id = self.state.next_request_id.fetch_add(1, Ordering::Relaxed);
        // Registered before the request goes out: the IDE may answer with a
        // frame before this function's caller ever polls the future.
        let (incoming_tx, incoming_rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
        self.state.register(request_id, incoming_tx);
        let guard = PendingGuard {
            state: self.state.clone(),
            request_id,
        };

        let url = request.url.clone();
        let accepted = session.request(proto::ProxyHttpRequest {
            request_id,
            method: request.method,
            url: request.url,
            headers: encode_headers(request.headers),
            body: request.body,
            follow_redirects: request.follow_redirects,
            timeout_millis: request
                .timeout
                .map(|timeout| timeout.as_millis().min(u128::from(u64::MAX)) as u64),
        });

        let executor = self.executor.clone();
        async move {
            // One round trip. The IDE answers as soon as it has taken the
            // request, so a peer with no handler for this — an older client, a
            // test double — fails here instead of leaving the caller waiting.
            match futures::future::select(
                Box::pin(accepted),
                executor.timer(RELAY_ACCEPT_TIMEOUT),
            )
            .await
            {
                futures::future::Either::Left((accepted, _)) => {
                    accepted
                        .with_context(|| format!("the connected IDE would not relay {url}"))?;
                }
                futures::future::Either::Right(_) => {
                    return Err(ProxyHttpRelayError::Network {
                        message: format!(
                            "the connected IDE did not take the request for {url} within {} seconds",
                            RELAY_ACCEPT_TIMEOUT.as_secs()
                        ),
                    }
                    .into_anyhow());
                }
            }

            let (outgoing_tx, outgoing_rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
            executor
                .spawn(forward_frames(
                    incoming_rx,
                    outgoing_tx,
                    executor.clone(),
                    guard,
                    url,
                ))
                .detach();
            Ok(outgoing_rx)
        }
        .boxed()
    }
}

/// Moves frames from the response handler to the reader, ending the transfer
/// if the IDE goes quiet.
///
/// The [`PendingGuard`] rides along so that whichever way this ends — the body
/// was dropped, the IDE disappeared, the transfer finished — the request stops
/// being in flight.
async fn forward_frames(
    mut incoming: mpsc::Receiver<Result<ProxyHttpFrame>>,
    mut outgoing: mpsc::Sender<Result<ProxyHttpFrame>>,
    executor: BackgroundExecutor,
    guard: PendingGuard,
    url: String,
) {
    let _guard = guard;
    loop {
        let frame = {
            let mut next_frame = incoming.next().fuse();
            let mut stalled = std::pin::pin!(executor.timer(FRAME_STALL_TIMEOUT).fuse());
            futures::select_biased! {
                frame = next_frame => frame,
                _ = stalled => {
                    let stalled = ProxyHttpRelayError::Network {
                        message: format!(
                            "the connected IDE stopped sending {url} for {} seconds",
                            FRAME_STALL_TIMEOUT.as_secs()
                        ),
                    };
                    log::warn!("{stalled}");
                    if outgoing.send(Err(stalled.into_anyhow())).await.is_err() {
                        log::debug!("nothing was left reading {url} when it stalled");
                    }
                    return;
                }
            }
        };
        let Some(frame) = frame else {
            return;
        };
        let terminal = frame.is_err();
        if outgoing.send(frame).await.is_err() {
            log::debug!("the reader of {url} went away; cancelling the relayed request");
            return;
        }
        if terminal {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use http_client::RemoteProxyHttpClient;
    use rpc::proto::{Envelope, EnvelopedMessage as _};

    /// Stands in for the IDE on the other end of the connection: takes the one
    /// `ProxyHttpRequest` the remote host sends and answers it with `frames`,
    /// waiting for each frame to be acknowledged before sending the next, the
    /// way the real handler does.
    async fn answer_one_request(
        from_server: &mut mpsc::UnboundedReceiver<Envelope>,
        to_server: &mpsc::UnboundedSender<Envelope>,
        frames: impl Fn(u64) -> Vec<proto::ProxyHttpResponse>,
    ) -> proto::ProxyHttpRequest {
        let mut next_id = 1000;
        let request = loop {
            let envelope = from_server.next().await.expect("the host should send");
            let envelope_id = envelope.id;
            if let Some(request) = proto::ProxyHttpRequest::from_envelope(envelope) {
                to_server
                    .unbounded_send(proto::Ack {}.into_envelope(next_id, Some(envelope_id), None))
                    .expect("the host should still be listening");
                next_id += 1;
                break request;
            }
        };

        for frame in frames(request.request_id) {
            let frame_id = next_id;
            next_id += 1;
            to_server
                .unbounded_send(frame.into_envelope(frame_id, None, None))
                .expect("the host should still be listening");
            loop {
                let envelope = from_server.next().await.expect("the host should answer");
                if envelope.responding_to == Some(frame_id) {
                    break;
                }
            }
        }
        request
    }

    fn header(name: &str, value: &str) -> proto::ProxyHttpHeader {
        proto::ProxyHttpHeader {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    #[gpui::test]
    async fn a_relayed_download_is_reassembled_from_its_frames(cx: &mut TestAppContext) {
        let (to_server, to_server_rx) = mpsc::unbounded::<Envelope>();
        let (from_server_tx, mut from_server) = mpsc::unbounded::<Envelope>();
        let session = cx.update(|cx| {
            RemoteClient::proto_client_from_channels(
                to_server_rx,
                from_server_tx,
                cx,
                "test-server",
                false,
            )
        });
        let transport = cx.update(|cx| ProxyHttpRelay::new(&session, cx));
        let http_client = RemoteProxyHttpClient::new(transport, None);

        let request = Request::builder()
            .uri("https://github.com/clangd/clangd/releases/download/18/clangd.zip")
            .header("accept", "application/octet-stream")
            .body(AsyncBody::empty())
            .expect("the request should build");
        let response = http_client.send(request);

        let served = answer_one_request(&mut from_server, &to_server, |request_id| {
            vec![
                proto::ProxyHttpResponse {
                    request_id,
                    status: 200,
                    headers: vec![header("content-length", "10")],
                    chunk: b"clang".to_vec(),
                    ..Default::default()
                },
                proto::ProxyHttpResponse {
                    request_id,
                    chunk: b"d.zip".to_vec(),
                    ..Default::default()
                },
                proto::ProxyHttpResponse {
                    request_id,
                    is_last: true,
                    ..Default::default()
                },
            ]
        });

        let (response, served) = futures::future::join(response, served).await;
        let mut response = response.expect("the relayed request should succeed");

        assert_eq!(
            served.url,
            "https://github.com/clangd/clangd/releases/download/18/clangd.zip"
        );
        assert_eq!(served.method, "GET");
        assert!(
            served
                .headers
                .iter()
                .any(|header| header.name == "accept" && header.value == "application/octet-stream")
        );

        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok()),
            Some("10")
        );
        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .expect("the body should read to the end");
        assert_eq!(body, b"clangd.zip");
    }

    /// ADR-F22 depends on this: a download the IDE's allowlist refused has to
    /// arrive here as an `EgressDenied` on the error chain, or the consent
    /// prompt never appears and the user sees a raw failure instead.
    #[gpui::test]
    async fn a_denial_by_the_ide_reaches_the_caller_as_an_egress_denial(cx: &mut TestAppContext) {
        let (to_server, to_server_rx) = mpsc::unbounded::<Envelope>();
        let (from_server_tx, mut from_server) = mpsc::unbounded::<Envelope>();
        let session = cx.update(|cx| {
            RemoteClient::proto_client_from_channels(
                to_server_rx,
                from_server_tx,
                cx,
                "test-server",
                false,
            )
        });
        let transport = cx.update(|cx| ProxyHttpRelay::new(&session, cx));
        let http_client = RemoteProxyHttpClient::new(transport, None);

        let request = Request::builder()
            .uri("https://api.github.com/repos/clangd/clangd/releases")
            .body(AsyncBody::empty())
            .expect("the request should build");
        let response = http_client.send(request);

        let served = answer_one_request(&mut from_server, &to_server, |request_id| {
            vec![proto::ProxyHttpResponse {
                request_id,
                is_last: true,
                error: Some(relay_error_to_proto(ProxyHttpRelayError::EgressDenied {
                    host: "api.github.com".into(),
                    url: "https://api.github.com/repos/clangd/clangd/releases".into(),
                })),
                ..Default::default()
            }]
        });

        let (response, _served) = futures::future::join(response, served).await;
        let Err(error) = response else {
            panic!("the request should have failed");
        };
        assert_eq!(
            http_client::egress_denied_hosts(&error),
            Some(vec![
                "api.github.com".to_string(),
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ]),
        );
    }

    /// ADR-F45: a remote host serving several IDE sessions sends each request on
    /// whichever session is picked when it is made, and the answer has to find
    /// its way back even though the relay is shared.
    #[gpui::test]
    async fn a_shared_relay_sends_on_the_session_it_is_given(cx: &mut TestAppContext) {
        let (_first_to_server, first_to_server_rx) = mpsc::unbounded::<Envelope>();
        let (first_from_server_tx, mut first_from_server) = mpsc::unbounded::<Envelope>();
        let (second_to_server, second_to_server_rx) = mpsc::unbounded::<Envelope>();
        let (second_from_server_tx, mut second_from_server) = mpsc::unbounded::<Envelope>();
        let first_session = cx.update(|cx| {
            RemoteClient::proto_client_from_channels(
                first_to_server_rx,
                first_from_server_tx,
                cx,
                "test-server-first",
                false,
            )
        });
        let second_session = cx.update(|cx| {
            RemoteClient::proto_client_from_channels(
                second_to_server_rx,
                second_from_server_tx,
                cx,
                "test-server-second",
                false,
            )
        });

        let chosen = Arc::new(Mutex::new(Some(second_session.clone())));
        let (relay, transport) = cx.update(|cx| {
            ProxyHttpRelay::new_with_session_source(
                {
                    let chosen = chosen.clone();
                    Arc::new(move || chosen.lock().clone())
                },
                cx,
            )
        });
        ProxyHttpRelay::attach(&relay, &first_session);
        ProxyHttpRelay::attach(&relay, &second_session);
        let http_client = RemoteProxyHttpClient::new(transport, None);

        let request = Request::builder()
            .uri("https://registry.npmjs.org/pyright")
            .body(AsyncBody::empty())
            .expect("the request should build");
        let response = http_client.send(request);
        let served = answer_one_request(&mut second_from_server, &second_to_server, |request_id| {
            vec![proto::ProxyHttpResponse {
                request_id,
                status: 200,
                chunk: b"{}".to_vec(),
                is_last: true,
                ..Default::default()
            }]
        });
        let (response, served) = futures::future::join(response, served).await;
        assert_eq!(served.url, "https://registry.npmjs.org/pyright");
        let mut response = response.expect("the relayed request should succeed");
        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .expect("the body should read to the end");
        assert_eq!(body, b"{}");

        // The first session only ever announced itself; the request did not go
        // out on it.
        cx.run_until_parked();
        while let Some(Some(envelope)) = first_from_server.next().now_or_never() {
            assert!(
                proto::ProxyHttpRequest::from_envelope(envelope).is_none(),
                "the request must go out on the session the source picked"
            );
        }

        *chosen.lock() = None;
        let request = Request::builder()
            .uri("https://registry.npmjs.org/pyright")
            .body(AsyncBody::empty())
            .expect("the request should build");
        assert!(
            http_client.send(request).await.is_err(),
            "with no IDE connected there is nothing to relay through"
        );
    }

    /// ADR-F45: a session whose connection died quietly would otherwise hold a
    /// download that picked it forever.
    #[gpui::test]
    async fn a_request_nobody_takes_fails_instead_of_hanging(cx: &mut TestAppContext) {
        let (_to_server, to_server_rx) = mpsc::unbounded::<Envelope>();
        let (from_server_tx, _from_server) = mpsc::unbounded::<Envelope>();
        let session = cx.update(|cx| {
            RemoteClient::proto_client_from_channels(
                to_server_rx,
                from_server_tx,
                cx,
                "test-server",
                false,
            )
        });
        let transport = cx.update(|cx| ProxyHttpRelay::new(&session, cx));
        let http_client = RemoteProxyHttpClient::new(transport, None);

        let request = Request::builder()
            .uri("https://registry.npmjs.org/pyright")
            .body(AsyncBody::empty())
            .expect("the request should build");
        let response = cx.executor().spawn(http_client.send(request));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(RELAY_ACCEPT_TIMEOUT + Duration::from_secs(1));
        assert!(response.await.is_err());
    }

    #[test]
    fn an_egress_denial_survives_the_wire_and_is_still_downcastable() {
        // ADR-F27/ADR-F22: this is the whole reason the denial travels as its
        // own message rather than as text. `language::binary_fetch_failure`
        // and `activity_indicator` find it by downcasting, so the round trip
        // through proto has to hand back the error type itself.
        let denied = ProxyHttpRelayError::EgressDenied {
            host: "github.com".into(),
            url: "https://github.com/clangd/clangd/releases".into(),
        };
        let restored = relay_error_from_proto(relay_error_to_proto(denied.clone()));
        assert_eq!(restored, denied);

        let error = restored.into_anyhow().context("fetching clangd");
        assert_eq!(
            http_client::egress_denied_hosts(&error),
            Some(vec![
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ]),
        );
    }

    #[test]
    fn the_other_error_kinds_keep_their_kind_and_message() {
        for error in [
            ProxyHttpRelayError::Network {
                message: "dns error: no such host".into(),
            },
            ProxyHttpRelayError::Other {
                message: "unsupported HTTP method `TRACE`".into(),
            },
        ] {
            let restored = relay_error_from_proto(relay_error_to_proto(error.clone()));
            assert_eq!(restored, error);
            assert_eq!(
                http_client::egress_denied_hosts(&restored.into_anyhow()),
                None
            );
        }
    }

    #[test]
    fn a_failure_kind_this_build_does_not_know_is_still_a_failure() {
        let unknown = relay_error_from_proto(proto::ProxyHttpError { kind: None });
        assert!(matches!(unknown, ProxyHttpRelayError::Other { .. }));
    }

    #[test]
    fn headers_round_trip_in_order() {
        let headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            ("user-agent".to_string(), "Zed-Server/0.1".to_string()),
        ];
        assert_eq!(decode_headers(encode_headers(headers.clone())), headers);
    }
}
