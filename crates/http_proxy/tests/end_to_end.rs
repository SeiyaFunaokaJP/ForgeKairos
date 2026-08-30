//! End-to-end tests for the proxy crate.
//!
//! Each test spawns a real proxy on `127.0.0.1:0` and makes real TCP
//! connections to it, optionally also spawning a tiny stub origin server
//! (or stub upstream proxy) to act as the destination. Everything is sync —
//! std::net + threads + std::time::Duration timeouts.

use futures::channel::mpsc;
use futures::stream::StreamExt;
use http_proxy::{
    Allowlist, DenyReason, DestinationPolicy, HostPattern, PolicyDecision, ProxyConfig, ProxyEvent,
    ProxyHandle, RequestMethod, RequestOutcome, UpstreamProxy,
};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Spin up a tiny TCP server that serves one connection: it reads the
/// client's first request (until `\r\n\r\n`), echoes a fixed HTTP
/// response, and returns the request bytes it saw.
fn spawn_echo_origin(response: &'static [u8]) -> (SocketAddr, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let join = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
        let buf = read_until_double_crlf(&mut sock);
        sock.write_all(response).unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        buf
    });
    (addr, join)
}

/// Spin up a stub upstream HTTP proxy that serves one connection: it reads
/// a CONNECT request, replies `200`, then acts like the requested origin —
/// reading one more request through the "tunnel" and echoing a fixed
/// response. Returns the CONNECT headers and the tunneled request bytes.
fn spawn_stub_upstream_proxy(
    tunnel_response: &'static [u8],
) -> (SocketAddr, thread::JoinHandle<(Vec<u8>, Vec<u8>)>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let join = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
        let connect_request = read_until_double_crlf(&mut sock);
        assert!(
            connect_request.starts_with(b"CONNECT "),
            "expected CONNECT, got: {:?}",
            String::from_utf8_lossy(&connect_request)
        );
        sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .unwrap();
        let tunneled_request = read_until_double_crlf(&mut sock);
        sock.write_all(tunnel_response).unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        (connect_request, tunneled_request)
    });
    (addr, join)
}

fn read_until_double_crlf(sock: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    buf
}

fn spawn_proxy(allowlist: Allowlist) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    spawn_proxy_with_upstream(allowlist, None)
}

fn spawn_proxy_with_upstream(
    allowlist: Allowlist,
    upstream: Option<UpstreamProxy>,
) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let proxy = ProxyHandle::spawn(ProxyConfig {
        allowlist,
        upstream,
        events: events_tx,
        policy: None,
        outer_policy: None,
    })
    .expect("proxy spawn");

    drain_ready(&proxy, &mut events_rx);
    (proxy, events_rx)
}

fn spawn_proxy_with_policy(
    policy: Arc<dyn DestinationPolicy>,
) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let proxy = ProxyHandle::spawn(ProxyConfig {
        // Deliberately the strictest allowlist there is: a custom policy must
        // replace it outright rather than intersect with it.
        allowlist: Allowlist::empty(),
        upstream: None,
        events: events_tx,
        policy: Some(policy),
        outer_policy: None,
    })
    .expect("proxy spawn");

    drain_ready(&proxy, &mut events_rx);
    (proxy, events_rx)
}

#[cfg(unix)]
fn spawn_unix_proxy(allowlist: Allowlist) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let proxy = ProxyHandle::spawn_unix_temp(ProxyConfig {
        allowlist,
        upstream: None,
        events: events_tx,
        policy: None,
        outer_policy: None,
    })
    .expect("proxy spawn");

    drain_ready(&proxy, &mut events_rx);
    (proxy, events_rx)
}

fn drain_ready(proxy: &ProxyHandle, events_rx: &mut mpsc::UnboundedReceiver<ProxyEvent>) {
    let ready = futures::executor::block_on(events_rx.next());
    match ready {
        Some(ProxyEvent::Ready { port }) => assert_eq!(port, proxy.port()),
        other => panic!("expected Ready event first, got {other:?}"),
    }
}

fn next_event(events: &mut mpsc::UnboundedReceiver<ProxyEvent>) -> ProxyEvent {
    futures::executor::block_on(events.next()).expect("events channel closed")
}

#[test]
fn connect_allowed_host_completes_tunnel_and_emits_events() {
    let (origin_addr, origin_join) = spawn_echo_origin(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    );

    // `localhost` isn't a permitted allowlist *pattern*, but
    // `Allowlist::any()` skips all policy checks (including the
    // forbidden-resolved-IP filter), which is what lets this test reach a
    // loopback origin. Denied paths are exercised in separate tests.
    let (proxy, mut events) = spawn_proxy(Allowlist::any());
    let target = format!("localhost:{}", origin_addr.port());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();

    let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    client.write_all(req.as_bytes()).unwrap();

    // Read the proxy's CONNECT response headers.
    let response = read_until_double_crlf(&mut client);
    let resp_text = String::from_utf8_lossy(&response);
    assert!(
        resp_text.starts_with("HTTP/1.1 200"),
        "expected 200 from proxy, got: {resp_text}"
    );

    // Send tunnel payload.
    client.write_all(b"hi origin\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());

    let origin_received = origin_join.join().unwrap();
    assert!(
        String::from_utf8_lossy(&origin_received).contains("hi origin"),
        "origin saw: {:?}",
        String::from_utf8_lossy(&origin_received)
    );

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            port,
            method,
            outcome,
        } => {
            assert_eq!(host, "localhost");
            assert_eq!(port, origin_addr.port());
            assert_eq!(method, RequestMethod::Connect);
            assert!(matches!(outcome, RequestOutcome::Allowed));
        }
        other => panic!("expected RequestAttempt, got {other:?}"),
    }
    match next_event(&mut events) {
        ProxyEvent::RequestCompleted { host, .. } => {
            assert_eq!(host, "localhost");
        }
        other => panic!("expected RequestCompleted, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn unix_listener_denied_host_returns_511() {
    let allowlist = Allowlist::from_patterns([HostPattern::parse("github.com").unwrap()]);
    let (proxy, mut events) = spawn_unix_proxy(allowlist);
    let socket_path = proxy
        .socket_path()
        .expect("Unix proxy socket path")
        .to_path_buf();

    let mut client = UnixStream::connect(socket_path).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("denied.example"));

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            port,
            method,
            outcome,
        } => {
            assert_eq!(host, "denied.example");
            assert_eq!(port, 443);
            assert_eq!(method, RequestMethod::Connect);
            assert!(matches!(outcome, RequestOutcome::Denied { .. }));
        }
        other => panic!("expected RequestAttempt(Denied), got {other:?}"),
    }
}

#[test]
fn connect_denied_host_returns_511_with_via_header() {
    let allowlist = Allowlist::from_patterns([HostPattern::parse("github.com").unwrap()]);
    let (proxy, mut events) = spawn_proxy(allowlist);

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("Via: 1.1 zed-sandbox-proxy"));
    assert!(response.contains("Proxy-Status: zed-sandbox-proxy"));
    assert!(response.contains("denied.example"));
    assert!(response.contains("not in this conversation's network allowlist"));

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            port,
            method,
            outcome,
        } => {
            assert_eq!(host, "denied.example");
            assert_eq!(port, 443);
            assert_eq!(method, RequestMethod::Connect);
            assert!(
                matches!(
                    outcome,
                    RequestOutcome::Denied {
                        reason: DenyReason::HostNotInAllowlist { .. }
                    }
                ),
                "outcome was {outcome:?}"
            );
        }
        other => panic!("expected RequestAttempt(Denied), got {other:?}"),
    }
}

#[test]
fn http_forward_denied_host_returns_511() {
    let allowlist = Allowlist::from_patterns([HostPattern::parse("github.com").unwrap()]);
    let (proxy, mut events) = spawn_proxy(allowlist);

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"GET http://denied.example/ HTTP/1.1\r\nHost: denied.example\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("not in this conversation's network allowlist"));

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            method: RequestMethod::Http(method),
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::HostNotInAllowlist { .. },
                },
            ..
        } => {
            assert_eq!(host, "denied.example");
            assert_eq!(method, "GET");
        }
        other => panic!("expected RequestAttempt(Denied), got {other:?}"),
    }
}

#[test]
fn ip_literal_connect_is_denied_for_pattern_allowlists() {
    let allowlist = Allowlist::from_patterns([HostPattern::parse("github.com").unwrap()]);
    let (proxy, mut events) = spawn_proxy(allowlist);

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT 1.2.3.4:443 HTTP/1.1\r\nHost: 1.2.3.4:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 511 "));
    assert!(response.contains("IP literal"));

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::IpLiteralRejected { target },
                },
            ..
        } => {
            assert_eq!(target, "1.2.3.4:443");
        }
        other => panic!("expected IpLiteralRejected, got {other:?}"),
    }
}

#[test]
fn ip_literal_connect_is_allowed_when_allowlist_allows_any() {
    // `Allowlist::any()` preserves unrestricted proxy semantics, IP literals included.
    let (origin_addr, origin_join) = spawn_echo_origin(b"HTTP/1.1 200 OK\r\n\r\n");
    let (proxy, mut events) = spawn_proxy(Allowlist::any());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    let target = format!("127.0.0.1:{}", origin_addr.port());
    let req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    client.write_all(req.as_bytes()).unwrap();

    let response = read_until_double_crlf(&mut client);
    let resp_text = String::from_utf8_lossy(&response);
    assert!(
        resp_text.starts_with("HTTP/1.1 200"),
        "expected 200 from proxy, got: {resp_text}"
    );

    client.write_all(b"ping\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());
    origin_join.join().unwrap();

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt { host, outcome, .. } => {
            assert_eq!(host, "127.0.0.1");
            assert!(matches!(outcome, RequestOutcome::Allowed));
        }
        other => panic!("expected RequestAttempt(Allowed), got {other:?}"),
    }
}

#[test]
fn host_resolving_to_loopback_is_denied_for_pattern_allowlists() {
    // DNS-rebinding protection end-to-end: even when a hostname slips past
    // the allowlist (here `localhost`, which patterns can't express but the
    // wire can carry), the proxy must deny anything that resolves into the
    // local machine rather than connect. The denial surfaces at the
    // allowlist layer for this probe; the resolved-IP layer behind it is
    // unit-tested in `connection.rs` (`plan_route` tests), since
    // exercising it end-to-end would require an allowlisted hostname whose
    // real DNS points at loopback.
    let allowlist = Allowlist::from_patterns([HostPattern::parse("github.com").unwrap()]);
    let (proxy, _events) = spawn_proxy(allowlist);

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT localhost:80 HTTP/1.1\r\nHost: localhost:80\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
}

#[test]
fn http_forward_allowed_rewrites_to_origin_form() {
    let (origin_addr, origin_join) = spawn_echo_origin(
        b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
    );

    let (proxy, mut events) = spawn_proxy(Allowlist::any());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    let req = format!(
        "GET http://localhost:{}/foo HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
        origin_addr.port(),
        origin_addr.port()
    );
    client.write_all(req.as_bytes()).unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let response_str = String::from_utf8_lossy(&response);
    assert!(response_str.contains("HTTP/1.1 200"));
    assert!(response_str.contains("hello world"));
    // Mirror what real HTTP clients do after a `Connection: close`
    // response: close the socket so the proxy's bidir-pump finishes the
    // client→upstream half. (Threads handle this fine — when the client's
    // socket goes out of scope the pump's read returns EOF.)
    drop(client);

    let origin_saw = origin_join.join().unwrap();
    let origin_saw_str = String::from_utf8_lossy(&origin_saw);
    // The absolute-form proxy request must reach the origin rewritten to
    // origin-form — many real servers don't accept absolute-form.
    assert!(
        origin_saw_str.starts_with("GET /foo HTTP/1.1\r\n"),
        "origin saw: {origin_saw_str}"
    );
    assert!(
        origin_saw_str.contains(&format!("Host: localhost:{}\r\n", origin_addr.port())),
        "origin saw: {origin_saw_str}"
    );

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            method: RequestMethod::Http(m),
            outcome: RequestOutcome::Allowed,
            ..
        } => {
            assert_eq!(m, "GET");
        }
        other => panic!("expected RequestAttempt(Allowed), got {other:?}"),
    }
    match next_event(&mut events) {
        ProxyEvent::RequestCompleted { .. } => {}
        other => panic!("expected RequestCompleted, got {other:?}"),
    }
}

#[test]
fn connect_chains_through_upstream_proxy_with_auth() {
    let (upstream_addr, upstream_join) = spawn_stub_upstream_proxy(b"tunnel says hi");
    let upstream = UpstreamProxy::parse(
        Some(&format!(
            "http://alice:s3cret@127.0.0.1:{}",
            upstream_addr.port()
        )),
        // The stub upstream is on loopback, which `proxyvars` implicitly
        // bypasses; the destination decides bypassing, and `proxied.example`
        // isn't local, so the chain is used.
        None,
    )
    .unwrap()
    .unwrap();

    let allowlist = Allowlist::from_patterns([HostPattern::parse("proxied.example").unwrap()]);
    let (proxy, mut events) = spawn_proxy_with_upstream(allowlist, Some(upstream));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT proxied.example:443 HTTP/1.1\r\nHost: proxied.example:443\r\n\r\n")
        .unwrap();

    let response = read_until_double_crlf(&mut client);
    let resp_text = String::from_utf8_lossy(&response);
    assert!(
        resp_text.starts_with("HTTP/1.1 200"),
        "expected 200 from proxy, got: {resp_text}"
    );

    client.write_all(b"client tunnel bytes\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut tunneled_back = Vec::new();
    let _ = client.read_to_end(&mut tunneled_back);
    assert_eq!(tunneled_back, b"tunnel says hi");

    let (connect_request, tunneled_request) = upstream_join.join().unwrap();
    let connect_text = String::from_utf8_lossy(&connect_request);
    assert!(
        connect_text.starts_with("CONNECT proxied.example:443 HTTP/1.1\r\n"),
        "upstream saw: {connect_text}"
    );
    // Basic auth from the upstream URL is injected on the CONNECT.
    assert!(
        connect_text.contains("Proxy-Authorization: Basic YWxpY2U6czNjcmV0\r\n"),
        "upstream saw: {connect_text}"
    );
    assert!(
        String::from_utf8_lossy(&tunneled_request).contains("client tunnel bytes"),
        "tunneled: {:?}",
        String::from_utf8_lossy(&tunneled_request)
    );

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            outcome: RequestOutcome::Allowed,
            ..
        } => assert_eq!(host, "proxied.example"),
        other => panic!("expected RequestAttempt(Allowed), got {other:?}"),
    }
}

#[test]
fn http_forward_chains_through_upstream_via_connect_tunnel() {
    // Plain HTTP through the upstream must also use a CONNECT tunnel pinned
    // to the approved host. Handing the upstream a routable absolute-form
    // stream would let keep-alive requests after the first reach hosts the
    // allowlist never approved.
    let (upstream_addr, upstream_join) =
        spawn_stub_upstream_proxy(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    let upstream = UpstreamProxy::parse(
        Some(&format!("http://127.0.0.1:{}", upstream_addr.port())),
        None,
    )
    .unwrap()
    .unwrap();

    let allowlist = Allowlist::from_patterns([HostPattern::parse("proxied.example").unwrap()]);
    let (proxy, _events) = spawn_proxy_with_upstream(allowlist, Some(upstream));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"GET http://proxied.example/data HTTP/1.1\r\nHost: proxied.example\r\n\r\n")
        .unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    let response_str = String::from_utf8_lossy(&response);
    assert!(
        response_str.contains("HTTP/1.1 200") && response_str.contains("ok"),
        "client got: {response_str}"
    );
    drop(client);

    let (connect_request, tunneled_request) = upstream_join.join().unwrap();
    let connect_text = String::from_utf8_lossy(&connect_request);
    assert!(
        connect_text.starts_with("CONNECT proxied.example:80 HTTP/1.1\r\n"),
        "upstream saw: {connect_text}"
    );
    // Inside the tunnel, the origin sees an origin-form request.
    let tunneled_text = String::from_utf8_lossy(&tunneled_request);
    assert!(
        tunneled_text.starts_with("GET /data HTTP/1.1\r\n"),
        "tunneled: {tunneled_text}"
    );
    assert!(
        tunneled_text.contains("Host: proxied.example\r\n"),
        "tunneled: {tunneled_text}"
    );
}

#[test]
fn dropping_handle_stops_listener() {
    let (proxy, _events) = spawn_proxy(Allowlist::any());
    let port = proxy.port();
    drop(proxy);

    // After Drop, the listener has been signaled and woken; new
    // connections should be refused.
    let result = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(500),
    );
    assert!(
        result.is_err(),
        "listener should have stopped after ProxyHandle drop, but new connection succeeded"
    );
}

/// A stand-in for this fork's egress-allowlist policy (ADR-F13): a set of
/// exact hostnames plus loopback, with loopback destinations reachable.
struct HostSetPolicy(&'static [&'static str]);

impl DestinationPolicy for HostSetPolicy {
    fn evaluate(&self, host: &str, _port: u16) -> PolicyDecision {
        if host == "localhost" || self.0.contains(&host) {
            PolicyDecision::Allow {
                permit_local_addresses: true,
            }
        } else {
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
                host: host.to_string(),
                explanation: format!("host '{host}' is not on the network allowlist"),
            })
        }
    }
}

#[test]
fn custom_policy_tunnels_connect_to_a_permitted_host() {
    let (origin_addr, origin_join) = spawn_echo_origin(
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
    );

    // The origin lives on loopback, which an empty `Allowlist` plus the
    // DNS-rebinding filter would deny twice over — so this also covers the
    // policy replacing both rather than intersecting with them.
    let (proxy, mut events) = spawn_proxy_with_policy(Arc::new(HostSetPolicy(&["example.com"])));
    let target = format!("localhost:{}", origin_addr.port());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    client.write_all(request.as_bytes()).unwrap();

    let response = read_until_double_crlf(&mut client);
    let response_text = String::from_utf8_lossy(&response);
    assert!(
        response_text.starts_with("HTTP/1.1 200"),
        "expected the tunnel to open, got: {response_text}"
    );

    client.write_all(b"hi origin\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());

    let origin_received = origin_join.join().unwrap();
    assert!(
        String::from_utf8_lossy(&origin_received).contains("hi origin"),
        "origin saw: {:?}",
        String::from_utf8_lossy(&origin_received)
    );

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            method,
            outcome,
            ..
        } => {
            assert_eq!(host, "localhost");
            assert_eq!(method, RequestMethod::Connect);
            assert!(matches!(outcome, RequestOutcome::Allowed));
        }
        other => panic!("expected RequestAttempt, got {other:?}"),
    }
}

#[test]
fn custom_policy_denies_connect_to_a_host_outside_the_allowlist() {
    let (proxy, mut events) = spawn_proxy_with_policy(Arc::new(HostSetPolicy(&["example.com"])));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT api.openai.com:443 HTTP/1.1\r\nHost: api.openai.com:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("api.openai.com"), "{response}");
    assert!(response.contains("network_allowlist.json"), "{response}");
    // The sandbox's framing must not leak into a non-sandbox denial.
    assert!(response.contains("Via: 1.1 zed-egress-proxy"), "{response}");
    assert!(!response.contains("sandbox"), "{response}");

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::NotPermittedByPolicy { .. },
                },
            ..
        } => assert_eq!(host, "api.openai.com"),
        other => panic!("expected RequestAttempt(Denied), got {other:?}"),
    }
}

#[test]
fn custom_policy_forwards_plain_http_to_a_permitted_host() {
    let (origin_addr, origin_join) =
        spawn_echo_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");

    let (proxy, mut events) = spawn_proxy_with_policy(Arc::new(HostSetPolicy(&[])));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    let request = format!(
        "GET http://localhost:{}/data HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
        origin_addr.port(),
        origin_addr.port()
    );
    client.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.contains("HTTP/1.1 200"), "client got: {response}");

    let origin_received = origin_join.join().unwrap();
    let origin_text = String::from_utf8_lossy(&origin_received);
    assert!(
        origin_text.starts_with("GET /data HTTP/1.1\r\n"),
        "origin saw: {origin_text}"
    );

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            method: RequestMethod::Http(method),
            outcome: RequestOutcome::Allowed,
            ..
        } => {
            assert_eq!(host, "localhost");
            assert_eq!(method, "GET");
        }
        other => panic!("expected an allowed HTTP attempt, got {other:?}"),
    }
}

#[test]
fn custom_policy_denies_plain_http_to_a_host_outside_the_allowlist() {
    let (proxy, mut events) = spawn_proxy_with_policy(Arc::new(HostSetPolicy(&["example.com"])));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(
            b"GET http://telemetry.example/collect HTTP/1.1\r\nHost: telemetry.example\r\n\r\n",
        )
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("telemetry.example"), "{response}");

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            method: RequestMethod::Http(_),
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::NotPermittedByPolicy { .. },
                },
            ..
        } => assert_eq!(host, "telemetry.example"),
        other => panic!("expected RequestAttempt(Denied), got {other:?}"),
    }
}

#[test]
fn custom_policy_decides_ip_literals_the_built_in_one_rejects_outright() {
    // The built-in policy denies every IP literal; this fork's allowlist can
    // list one, so a permitted literal must tunnel and an unlisted one must not.
    let (origin_addr, origin_join) =
        spawn_echo_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");

    struct LiteralPolicy;
    impl DestinationPolicy for LiteralPolicy {
        fn evaluate(&self, host: &str, _port: u16) -> PolicyDecision {
            if host == "127.0.0.1" {
                PolicyDecision::Allow {
                    permit_local_addresses: true,
                }
            } else {
                PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
                    host: host.to_string(),
                    explanation: format!("host '{host}' is not on the network allowlist"),
                })
            }
        }
    }

    let (proxy, _events) = spawn_proxy_with_policy(Arc::new(LiteralPolicy));
    let target = format!("127.0.0.1:{}", origin_addr.port());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
    let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
    client.write_all(request.as_bytes()).unwrap();
    let response = read_until_double_crlf(&mut client);
    assert!(
        String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
        "an allowlisted IP literal must tunnel: {:?}",
        String::from_utf8_lossy(&response)
    );
    client.write_all(b"hi\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());
    origin_join.join().unwrap();

    let mut denied = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    denied.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    denied
        .write_all(b"CONNECT 10.0.0.5:443 HTTP/1.1\r\nHost: 10.0.0.5:443\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    denied.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
}

/// Stand-in for this fork's editor-wide egress allowlist when it is used as the
/// ceiling around a sandbox's own host list: one permitted host, and loopback /
/// private space permitted the way that allowlist permits them.
struct OuterAllowlist(&'static str);

impl DestinationPolicy for OuterAllowlist {
    fn evaluate(&self, host: &str, _port: u16) -> PolicyDecision {
        if host == self.0 {
            PolicyDecision::Allow {
                permit_local_addresses: true,
            }
        } else {
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
                host: host.to_string(),
                explanation: format!("host '{host}' is not on the network allowlist"),
            })
        }
    }
}

fn spawn_proxy_nested_within(
    allowlist: Allowlist,
    outer_policy: Arc<dyn DestinationPolicy>,
) -> (ProxyHandle, mpsc::UnboundedReceiver<ProxyEvent>) {
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let proxy = ProxyHandle::spawn(ProxyConfig {
        allowlist,
        upstream: None,
        events: events_tx,
        policy: None,
        outer_policy: Some(outer_policy),
    })
    .expect("proxy spawn");

    drain_ready(&proxy, &mut events_rx);
    (proxy, events_rx)
}

#[test]
fn a_host_the_outer_policy_refuses_is_denied_even_though_the_allowlist_has_it() {
    let allowlist = Allowlist::from_patterns([HostPattern::parse("denied.example").unwrap()]);
    let (proxy, mut events) =
        spawn_proxy_nested_within(allowlist, Arc::new(OuterAllowlist("github.com")));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\nHost: denied.example:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    // The denial names the policy that has to be widened, which for an
    // outer-policy refusal is the editor-wide allowlist, not the sandbox's.
    assert!(response.contains("Via: 1.1 zed-egress-proxy"), "{response}");
    assert!(response.contains("network_allowlist.json"), "{response}");

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::NotPermittedByPolicy { .. },
                },
            ..
        } => assert_eq!(host, "denied.example"),
        other => panic!("expected RequestAttempt(NotPermittedByPolicy), got {other:?}"),
    }
}

#[test]
fn a_host_both_policies_permit_tunnels_through() {
    let (origin_addr, origin_join) =
        spawn_echo_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");

    // As in `connect_allowed_host_completes_tunnel_and_emits_events`, the
    // loopback origin is only reachable because `Allowlist::any()` waives the
    // sandbox's own checks; here the outer policy has to permit it too.
    let (proxy, mut events) =
        spawn_proxy_nested_within(Allowlist::any(), Arc::new(OuterAllowlist("localhost")));
    let target = format!("localhost:{}", origin_addr.port());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .unwrap();

    let response = read_until_double_crlf(&mut client);
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 from proxy, got: {response}"
    );
    client.write_all(b"hi origin\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());
    origin_join.join().unwrap();

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt { host, outcome, .. } => {
            assert_eq!(host, "localhost");
            assert!(matches!(outcome, RequestOutcome::Allowed));
        }
        other => panic!("expected RequestAttempt, got {other:?}"),
    }
}

/// An `allow_all_hosts` sandbox reaches the proxy as `Allowlist::any()`: every
/// sandbox-side check waived, the ceiling left deciding alone. A host the
/// ceiling doesn't list is still refused — the point of proxying that grant at
/// all rather than handing it the raw network.
#[test]
fn an_allow_any_sandbox_is_still_refused_a_host_the_outer_policy_lacks() {
    let (proxy, mut events) =
        spawn_proxy_nested_within(Allowlist::any(), Arc::new(OuterAllowlist("github.com")));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT evil.example:443 HTTP/1.1\r\nHost: evil.example:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
    assert!(response.contains("network allowlist"), "{response}");

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt {
            host,
            outcome:
                RequestOutcome::Denied {
                    reason: DenyReason::NotPermittedByPolicy { .. },
                },
            ..
        } => assert_eq!(host, "evil.example"),
        other => panic!("expected RequestAttempt(NotPermittedByPolicy), got {other:?}"),
    }
}

/// …and an IP literal, which `Allowlist::any()` alone is defined to permit, is
/// refused on the same grounds.
#[test]
fn an_allow_any_sandbox_is_still_refused_an_ip_literal_the_outer_policy_lacks() {
    let (proxy, _events) =
        spawn_proxy_nested_within(Allowlist::any(), Arc::new(OuterAllowlist("github.com")));

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(b"CONNECT 10.0.0.5:443 HTTP/1.1\r\nHost: 10.0.0.5:443\r\n\r\n")
        .unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 511 "),
        "expected 511, got: {response}"
    );
}

/// The half that has to survive the nesting: an unrestricted sandbox has always
/// been able to reach the local machine, and this fork's egress allowlist
/// exempts loopback, so an IP-literal loopback target still connects.
#[test]
fn an_allow_any_sandbox_still_reaches_loopback_the_outer_policy_permits() {
    let (origin_addr, origin_join) =
        spawn_echo_origin(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let (proxy, mut events) =
        spawn_proxy_nested_within(Allowlist::any(), Arc::new(OuterAllowlist("127.0.0.1")));
    let target = format!("127.0.0.1:{}", origin_addr.port());

    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    client.set_read_timeout(Some(TEST_TIMEOUT)).unwrap();
    client.set_write_timeout(Some(TEST_TIMEOUT)).unwrap();
    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .unwrap();

    let response = read_until_double_crlf(&mut client);
    let response = String::from_utf8_lossy(&response);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200 from proxy, got: {response}"
    );
    client.write_all(b"hi origin\r\n\r\n").unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let _ = client.read_to_end(&mut Vec::new());
    origin_join.join().unwrap();

    match next_event(&mut events) {
        ProxyEvent::RequestAttempt { host, outcome, .. } => {
            assert_eq!(host, "127.0.0.1");
            assert!(matches!(outcome, RequestOutcome::Allowed));
        }
        other => panic!("expected RequestAttempt, got {other:?}"),
    }
}
