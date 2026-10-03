//! Relaying HTTP through another machine. See `.forgekairos-docs/DESIGN.md`
//! §4.27 (ADR-F27).
//!
//! A remote host that cannot reach the internet still has to fetch language
//! servers, node, extension archives and agent binaries. [`RemoteProxyHttpClient`]
//! is the [`HttpClient`] it uses instead of one that opens its own sockets:
//! every request is handed to a [`ProxyHttpTransport`], which carries it to the
//! connected IDE, and the IDE runs it through *its* client — allowlist
//! (ADR-F03), consent flow (ADR-F22) and proxy included.
//!
//! The transport lives outside this crate because it speaks the RPC protocol,
//! and `http_client` cannot depend on `rpc` (`gpui` depends on this crate).
//! What stays here is the part that has nothing to do with the wire: turning a
//! `http::Request` into something sendable, reassembling the chunked response
//! behind an [`AsyncBody`], and restoring [`EgressDenied`] so a denial that
//! happened on the client is classified on the remote exactly as a local one
//! would be.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::{AsyncRead, AsyncReadExt as _, FutureExt as _, StreamExt as _};
use http::{HeaderMap, HeaderValue, Request, Response};

use crate::egress::{EgressDenied, egress_denied_from_message};
use crate::{AsyncBody, HttpClient, RedirectPolicy, RequestTimeout};

/// Body bytes carried by a single response frame. Large enough that a
/// multi-megabyte download does not pay per-frame overhead 10000 times over,
/// small enough that no single frame keeps the RPC channel busy for long.
pub const PROXY_HTTP_CHUNK_SIZE: usize = 256 * 1024;

/// A request to run on the other end of the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHttpRelayRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether the far end should follow redirects on our behalf. It checks
    /// every hop against its own allowlist either way.
    pub follow_redirects: bool,
    /// The deadline the caller asked for, if any. Carried across so a request
    /// built with [`crate::HttpRequestExt::timeout`] keeps it rather than
    /// falling back on whatever the transport's own stall detection allows.
    pub timeout: Option<Duration>,
}

/// One piece of the response, in the order the far end produces it: exactly
/// one [`ProxyHttpFrame::Head`] followed by zero or more
/// [`ProxyHttpFrame::Chunk`]s. The end of the body is the end of the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyHttpFrame {
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Chunk(Bytes),
}

/// Why a relayed request failed, kept apart by kind so that the caller on the
/// remote host can act on it the same way it would act on a local failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyHttpRelayError {
    /// The IDE's egress allowlist refused the request (ADR-F03). Restored into
    /// an [`EgressDenied`] by [`ProxyHttpRelayError::into_anyhow`], which is
    /// what lets `language::binary_fetch_failure` and
    /// [`crate::egress_denied_hosts`] route the failure into the ADR-F22
    /// consent flow.
    EgressDenied { host: String, url: String },
    /// The request reached the network and failed there (DNS, TCP, TLS, a
    /// dropped connection), or the relay itself lost its peer.
    Network { message: String },
    /// Anything else: a malformed URL, a header the far end could not build.
    Other { message: String },
}

impl ProxyHttpRelayError {
    /// Classifies an error raised by the IDE's own [`HttpClient`] so it can be
    /// sent back over the relay.
    ///
    /// Walks the chain for [`EgressDenied`] and, failing that, looks for the
    /// marker line — a denial that already crossed one stringifying boundary
    /// (the extension WASM host) still has to arrive here as a denial.
    pub fn from_anyhow(error: &anyhow::Error) -> Self {
        if let Some(denied) = error
            .chain()
            .find_map(|error| error.downcast_ref::<EgressDenied>())
        {
            return ProxyHttpRelayError::EgressDenied {
                host: denied.host.clone(),
                url: denied.url.clone(),
            };
        }
        let message = format!("{error:#}");
        if let Some(denied) = egress_denied_from_message(&message) {
            return ProxyHttpRelayError::EgressDenied {
                host: denied.host,
                url: denied.url,
            };
        }
        if error
            .chain()
            .any(|error| error.downcast_ref::<std::io::Error>().is_some())
        {
            return ProxyHttpRelayError::Network { message };
        }
        ProxyHttpRelayError::Other { message }
    }

    /// Rebuilds the error on the remote host. An [`EgressDenied`] is put back
    /// on the chain as itself rather than as text, because every consent flow
    /// downcasts to it.
    pub fn into_anyhow(self) -> anyhow::Error {
        match self {
            ProxyHttpRelayError::EgressDenied { host, url } => {
                anyhow::Error::new(EgressDenied { host, url })
                    .context("the IDE refused to make this request on this host's behalf")
            }
            ProxyHttpRelayError::Network { message } => {
                anyhow::anyhow!("{message}").context("relayed request failed on the IDE's network")
            }
            ProxyHttpRelayError::Other { message } => {
                anyhow::anyhow!("{message}").context("the IDE could not run the relayed request")
            }
        }
    }
}

impl std::fmt::Display for ProxyHttpRelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyHttpRelayError::EgressDenied { host, url } => {
                write!(
                    f,
                    "{}",
                    EgressDenied {
                        host: host.clone(),
                        url: url.clone(),
                    }
                )
            }
            ProxyHttpRelayError::Network { message } | ProxyHttpRelayError::Other { message } => {
                write!(f, "{message}")
            }
        }
    }
}

/// The response frames of one relayed request, in order.
///
/// A channel rather than a boxed stream because the frames end up behind an
/// [`AsyncBody`], which requires a `Sync` reader — few stream combinators are.
/// The bound also gives the transport its backpressure: the far end cannot run
/// further ahead of the reader than the channel is deep.
pub type ProxyHttpFrames = mpsc::Receiver<anyhow::Result<ProxyHttpFrame>>;

/// Carries a request to the machine that is allowed to make it, and brings the
/// response back frame by frame.
///
/// The channel closes when the body ends. An error in it is terminal; dropping
/// the receiver cancels the transfer.
pub trait ProxyHttpTransport: Send + Sync + 'static {
    fn send(
        &self,
        request: ProxyHttpRelayRequest,
    ) -> BoxFuture<'static, anyhow::Result<ProxyHttpFrames>>;
}

/// The [`HttpClient`] a remote host uses when its own network cannot be
/// reached (ADR-F27). Every request goes to the connected IDE instead.
pub struct RemoteProxyHttpClient {
    transport: Arc<dyn ProxyHttpTransport>,
    user_agent: Option<HeaderValue>,
}

impl RemoteProxyHttpClient {
    pub fn new(transport: Arc<dyn ProxyHttpTransport>, user_agent: Option<HeaderValue>) -> Self {
        Self {
            transport,
            user_agent,
        }
    }
}

impl HttpClient for RemoteProxyHttpClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.user_agent.as_ref()
    }

    /// Always `None`: a proxy configured on this host would not be reachable
    /// from the IDE, and the IDE applies its own proxy to the relayed request.
    fn proxy(&self) -> Option<&url::Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let transport = self.transport.clone();
        async move { relay(transport, request).await }.boxed()
    }
}

async fn relay(
    transport: Arc<dyn ProxyHttpTransport>,
    request: Request<AsyncBody>,
) -> anyhow::Result<Response<AsyncBody>> {
    let (parts, body) = request.into_parts();
    let url = parts.uri.to_string();

    // The far end re-checks every redirect hop against its allowlist, so all
    // this carries is whether the caller wanted hops followed at all.
    let follow_redirects = !matches!(
        parts.extensions.get::<RedirectPolicy>(),
        Some(RedirectPolicy::NoFollow)
    );

    let timeout = parts
        .extensions
        .get::<RequestTimeout>()
        .map(|timeout| timeout.0);

    let body = read_to_end(body)
        .await
        .with_context(|| format!("reading the request body for {url}"))?;

    let mut frames = transport
        .send(ProxyHttpRelayRequest {
            method: parts.method.to_string(),
            url: url.clone(),
            headers: encode_headers(&parts.headers),
            body,
            follow_redirects,
            timeout,
        })
        .await
        .with_context(|| format!("relaying {url} through the connected IDE"))?;

    let head = frames
        .next()
        .await
        .unwrap_or_else(|| {
            Err(anyhow::anyhow!(
                "the IDE closed the relay without responding"
            ))
        })
        .with_context(|| format!("relaying {url} through the connected IDE"))?;
    let ProxyHttpFrame::Head { status, headers } = head else {
        anyhow::bail!("the IDE sent body bytes for {url} before the response head");
    };

    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(name, value);
    }
    response
        .body(AsyncBody::from_reader(RelayedBody {
            frames,
            pending: Bytes::new(),
            done: false,
        }))
        .with_context(|| format!("rebuilding the relayed response for {url}"))
}

/// Reads the whole request body into memory.
///
/// Relayed requests are LSP/node/extension downloads and the small JSON calls
/// that precede them, so a body large enough for this to matter does not
/// occur; the wire format has no upload streaming either way.
async fn read_to_end(mut body: AsyncBody) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn encode_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

/// The response body, read out of the frames as the caller asks for bytes.
struct RelayedBody {
    frames: ProxyHttpFrames,
    pending: Bytes,
    done: bool,
}

impl AsyncRead for RelayedBody {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            if !this.pending.is_empty() {
                let taken = this.pending.split_to(this.pending.len().min(buf.len()));
                buf[..taken.len()].copy_from_slice(&taken);
                return Poll::Ready(Ok(taken.len()));
            }
            if this.done {
                return Poll::Ready(Ok(0));
            }
            match this.frames.poll_next_unpin(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    return Poll::Ready(Ok(0));
                }
                Poll::Ready(Some(Ok(ProxyHttpFrame::Chunk(chunk)))) => this.pending = chunk,
                Poll::Ready(Some(Ok(ProxyHttpFrame::Head { .. }))) => {
                    this.done = true;
                    return Poll::Ready(Err(std::io::Error::other(
                        "the IDE sent a second response head on the same relayed request",
                    )));
                }
                Poll::Ready(Some(Err(error))) => {
                    this.done = true;
                    return Poll::Ready(Err(std::io::Error::other(format!("{error:#}"))));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HttpRequestExt as _;
    use futures::SinkExt as _;
    use parking_lot::Mutex;

    /// A transport that answers from a script instead of a peer, so the client
    /// can be exercised without an RPC connection.
    struct ScriptedTransport {
        frames: Mutex<Option<Vec<anyhow::Result<ProxyHttpFrame>>>>,
        seen: Mutex<Option<ProxyHttpRelayRequest>>,
    }

    impl ScriptedTransport {
        fn new(frames: Vec<anyhow::Result<ProxyHttpFrame>>) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(Some(frames)),
                seen: Mutex::new(None),
            })
        }
    }

    impl ProxyHttpTransport for ScriptedTransport {
        fn send(
            &self,
            request: ProxyHttpRelayRequest,
        ) -> BoxFuture<'static, anyhow::Result<ProxyHttpFrames>> {
            *self.seen.lock() = Some(request);
            let frames = self.frames.lock().take().unwrap_or_default();
            async move {
                let (mut sender, receiver) = mpsc::channel(frames.len().max(1));
                for frame in frames {
                    sender.send(frame).await.context("the receiver is live")?;
                }
                Ok(receiver)
            }
            .boxed()
        }
    }

    fn head(status: u16) -> ProxyHttpFrame {
        ProxyHttpFrame::Head {
            status,
            headers: vec![("content-type".into(), "application/octet-stream".into())],
        }
    }

    #[test]
    fn the_request_is_described_to_the_transport_and_the_chunks_are_rejoined() {
        let transport = ScriptedTransport::new(vec![
            Ok(head(200)),
            Ok(ProxyHttpFrame::Chunk(Bytes::from_static(b"clang"))),
            Ok(ProxyHttpFrame::Chunk(Bytes::from_static(b"d.tar"))),
        ]);
        let client = RemoteProxyHttpClient::new(transport.clone(), None);

        let request = Request::builder()
            .method("POST")
            .uri("https://github.com/clangd/clangd/releases")
            .header("accept", "application/octet-stream")
            .extension(RedirectPolicy::FollowAll)
            .timeout(Duration::from_secs(30))
            .body(AsyncBody::from("query".to_string()))
            .expect("the request should build");

        let mut response = futures::executor::block_on(client.send(request))
            .expect("the relayed request should succeed");

        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/octet-stream")
        );

        let mut body = Vec::new();
        futures::executor::block_on(response.body_mut().read_to_end(&mut body))
            .expect("the body should read to the end");
        assert_eq!(body, b"clangd.tar");

        let seen = transport.seen.lock().clone().expect("a request was sent");
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.url, "https://github.com/clangd/clangd/releases");
        assert_eq!(seen.body, b"query");
        assert!(seen.follow_redirects);
        assert_eq!(seen.timeout, Some(Duration::from_secs(30)));
        assert!(
            seen.headers
                .contains(&("accept".to_string(), "application/octet-stream".to_string()))
        );
    }

    #[test]
    fn a_no_follow_policy_is_carried_across() {
        let transport = ScriptedTransport::new(vec![Ok(head(302))]);
        let client = RemoteProxyHttpClient::new(transport.clone(), None);
        let request = Request::builder()
            .uri("https://api.github.com/repos/clangd/clangd/releases")
            .extension(RedirectPolicy::NoFollow)
            .body(AsyncBody::empty())
            .expect("the request should build");

        futures::executor::block_on(client.send(request)).expect("the request should succeed");

        let seen = transport.seen.lock().clone().expect("a request was sent");
        assert!(!seen.follow_redirects);
    }

    #[test]
    fn a_denial_before_the_head_is_restored_as_an_egress_denial() {
        // ADR-F27/ADR-F22: this is the whole point of giving the denial its
        // own variant — the consent flow on the remote host finds it by
        // downcasting, so it has to arrive as the error type, not as text.
        let transport = ScriptedTransport::new(vec![Err(ProxyHttpRelayError::EgressDenied {
            host: "github.com".into(),
            url: "https://github.com/clangd/clangd/releases".into(),
        }
        .into_anyhow())]);
        let client = RemoteProxyHttpClient::new(transport, None);
        let request = Request::builder()
            .uri("https://github.com/clangd/clangd/releases")
            .body(AsyncBody::empty())
            .expect("the request should build");

        let Err(error) = futures::executor::block_on(client.send(request)) else {
            panic!("the request should have been denied");
        };

        assert_eq!(
            crate::egress_denied_hosts(&error),
            Some(vec![
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ]),
        );
    }

    #[test]
    fn a_relay_error_kind_survives_the_round_trip_through_anyhow() {
        for error in [
            ProxyHttpRelayError::Network {
                message: "dns error: no such host".into(),
            },
            ProxyHttpRelayError::Other {
                message: "invalid header value".into(),
            },
        ] {
            let restored = error.clone().into_anyhow();
            assert_eq!(crate::egress_denied_hosts(&restored), None);
            assert!(format!("{restored:#}").contains(&error.to_string()));
        }
    }

    #[test]
    fn an_egress_denial_that_already_became_a_string_is_still_classified() {
        let denied = EgressDenied {
            host: "registry.npmjs.org".into(),
            url: "https://registry.npmjs.org/typescript".into(),
        };
        let stringified = anyhow::anyhow!(
            "{}\n{}",
            crate::egress_denied_marker_line(&denied),
            "download failed"
        );
        assert_eq!(
            ProxyHttpRelayError::from_anyhow(&stringified),
            ProxyHttpRelayError::EgressDenied {
                host: denied.host.clone(),
                url: denied.url.clone(),
            }
        );
        assert_eq!(
            ProxyHttpRelayError::from_anyhow(&anyhow::Error::new(denied.clone())),
            ProxyHttpRelayError::EgressDenied {
                host: denied.host,
                url: denied.url,
            }
        );
    }

    #[test]
    fn an_error_that_arrives_mid_body_stops_the_reader() {
        // A client that disconnects halfway through a download shows up here:
        // the head and some bytes made it, and then the transport reports the
        // loss instead of ending the body cleanly on a truncated file.
        let transport = ScriptedTransport::new(vec![
            Ok(head(200)),
            Ok(ProxyHttpFrame::Chunk(Bytes::from_static(b"half"))),
            Err(ProxyHttpRelayError::Network {
                message: "connection lost".into(),
            }
            .into_anyhow()),
        ]);
        let client = RemoteProxyHttpClient::new(transport, None);
        let request = Request::builder()
            .uri("https://example.com/x")
            .body(AsyncBody::empty())
            .expect("the request should build");
        let mut response =
            futures::executor::block_on(client.send(request)).expect("the head arrives");

        let mut body = Vec::new();
        let error = futures::executor::block_on(response.body_mut().read_to_end(&mut body))
            .expect_err("the body ends in an error");
        assert!(error.to_string().contains("connection lost"), "{error}");
    }
}
