//! The loopback forward proxy this fork points agent-harness child processes
//! at, so that their runtime traffic is judged by the same allowlist
//! [`http_client::AllowlistedHttpClient`] applies to the app's own requests.
//! See `docs/jp-enhance/DESIGN.md` ADR-F13 (F2-7).
//!
//! ACP agents, MCP servers and npm run in their own processes, on the far side
//! of a process boundary the in-process HTTP decorator cannot see across
//! (ADR-F03 §4.9, layers 2 and 3). Pointing them at a proxy whose destination
//! policy is that same allowlist brings them inside it — **cooperatively**: a
//! child that ignores `HTTP_PROXY`/`HTTPS_PROXY`, or that opens a raw socket,
//! is not constrained by this. It closes the honest path, not every path.
//!
//! The wire protocol, the connection handling and the destination pinning all
//! come from `http_proxy`, the crate the terminal sandbox already uses. Only
//! the policy differs, which is what [`http_proxy::DestinationPolicy`] exists
//! for: the sandbox denies IP literals and hostnames resolving into
//! loopback/private space, while this fork's allowlist deliberately permits
//! both (a local model server, a host listed by address).

use std::sync::Arc;

use http_client::{EgressPolicyHandle, RequestHost, Url};
use http_proxy::{
    Allowlist, DenyReason, DestinationPolicy, PolicyDecision, ProxyConfig, ProxyEvent, ProxyHandle,
    RequestOutcome, UpstreamProxy,
};

/// Owns the listener for one process and keeps it in step with the policy that
/// governs it.
///
/// In the IDE that policy is `network_allowlist.json`, and this is held by the
/// task watching it, so the listener's lifetime is that task's. In
/// `remote_server` the policy arrives over the wire instead (ADR-F14) and this
/// is held by the process-wide host that every project of that process shares
/// (ADR-F45) — not by any one project, since closing one project must not take
/// the endpoint away from the others. The listener binds the *remote* host's
/// loopback, since that is the only address the child processes spawned over
/// there can reach.
pub struct Supervisor {
    /// The user's own HTTP proxy, if they have one. The fork's proxy chains
    /// through it rather than around it, so a machine that only reaches the
    /// internet via a corporate proxy keeps working. Captured once at startup:
    /// changing Zed's `proxy` setting takes effect on the next launch, the same
    /// as it does for the app's own HTTP client.
    upstream: Option<UpstreamProxy>,
    running: Option<ProxyHandle>,
}

impl Supervisor {
    pub fn new(upstream_proxy: Option<&Url>) -> Self {
        let upstream = match UpstreamProxy::parse(
            upstream_proxy.map(Url::as_str),
            http_client::read_no_proxy_from_env().as_deref(),
        ) {
            Ok(upstream) => upstream,
            Err(error) => {
                // Chaining is a convenience; failing to parse the user's proxy
                // must not stop the allowlist from being enforced.
                log::warn!("egress proxy: ignoring an unusable upstream proxy: {error:#}");
                None
            }
        };
        Self {
            upstream,
            running: None,
        }
    }

    /// Starts the listener the first time the feature is on, stops it when it
    /// is turned off, and publishes the endpoint the spawn paths read.
    pub fn apply(&mut self, enabled: bool, policy: &EgressPolicyHandle) {
        if !enabled {
            if self.running.take().is_some() {
                http_client::set_child_process_proxy(None);
                log::info!(
                    "egress proxy: stopped; child processes are no longer pointed at it \
                     (proxy_child_processes is false)"
                );
            }
            return;
        }
        if self.running.is_some() {
            return;
        }

        match start(policy.clone(), self.upstream.clone()) {
            Ok((handle, endpoint)) => {
                log::info!("egress proxy: listening on {endpoint} for agent child processes");
                http_client::set_child_process_proxy(Some(endpoint));
                self.running = Some(handle);
            }
            Err(error) => {
                // Fail open on the *proxy*, never on the allowlist: the app's
                // own requests are still gated, and the child processes are
                // exactly as unconstrained as they were before F2-7.
                log::error!(
                    "egress proxy: could not start; agent child processes will not be \
                     proxied: {error:#}"
                );
            }
        }
    }
}

/// Dropping the supervisor stops the listener, so the endpoint it published
/// has to go with it — otherwise the spawn paths would keep handing child
/// processes a port that no longer answers. There is one supervisor per
/// process, which is what makes withdrawing a process-wide slot from `drop`
/// safe.
impl Drop for Supervisor {
    fn drop(&mut self) {
        if self.running.is_some() {
            http_client::set_child_process_proxy(None);
        }
    }
}

fn start(
    policy: EgressPolicyHandle,
    upstream: Option<UpstreamProxy>,
) -> anyhow::Result<(ProxyHandle, Url)> {
    let (events, receiver) = futures::channel::mpsc::unbounded();
    let handle = ProxyHandle::spawn(ProxyConfig {
        // Unused — `policy` replaces it outright. `empty()` rather than `any()`
        // so that a future code path which dropped the policy would deny
        // everything rather than open everything.
        allowlist: Allowlist::empty(),
        upstream,
        events,
        policy: Some(Arc::new(AllowlistPolicy { policy })),
        // The allowlist above *is* the ceiling here; there is no outer policy
        // to nest it inside, unlike the terminal sandbox's proxy.
        outer_policy: None,
    })?;
    let endpoint = Url::parse(&format!("http://127.0.0.1:{}", handle.port()))?;
    log_events(receiver);
    Ok((handle, endpoint))
}

/// Drains the proxy's event stream on a thread of its own, which exits when the
/// proxy is dropped and the channel closes. Denials are already logged at
/// `warn` by [`AllowlistPolicy`]; this is the allowed-traffic counterpart, kept
/// at `debug` so it can be turned on per-scope when diagnosing an agent.
fn log_events(events: futures::channel::mpsc::UnboundedReceiver<ProxyEvent>) {
    let spawned = std::thread::Builder::new()
        .name("egress-proxy-events".to_string())
        .spawn(move || {
            futures::executor::block_on(async move {
                use futures::StreamExt as _;
                let mut events = events;
                while let Some(event) = events.next().await {
                    match event {
                        ProxyEvent::Ready { .. } => {}
                        ProxyEvent::RequestAttempt {
                            host,
                            port,
                            method,
                            outcome: RequestOutcome::Allowed,
                        } => log::debug!("egress proxy: {} {host}:{port}", method.as_str()),
                        // Denials carry their reason from the policy, which has
                        // already logged them at `warn`.
                        ProxyEvent::RequestAttempt { .. } => {}
                        ProxyEvent::RequestCompleted {
                            host,
                            port,
                            method,
                            bytes_to_remote,
                            bytes_from_remote,
                            duration_ms,
                        } => log::debug!(
                            "egress proxy: completed {} {host}:{port} sent={bytes_to_remote} \
                             recv={bytes_from_remote} duration={duration_ms}ms",
                            method.as_str()
                        ),
                    }
                }
            })
        });
    if let Err(error) = spawned {
        log::warn!("egress proxy: could not start the event logger: {error}");
    }
}

/// Judges a child process's destinations by the user's `allowed_hosts` plus
/// loopback — [`EgressPolicyHandle::allows`], the same check the app's own
/// requests get.
///
/// [`http_client::BUILTIN_ALLOWLIST`] is deliberately *not* consulted. Those
/// entries are path-scoped on purpose (`api.zed.dev/extensions` and nothing
/// else on that host, because telemetry shares it), and a CONNECT tunnel
/// carries no path — TLS is not decrypted here. Applying them to plain HTTP
/// only would make the same host reachable or not depending on the scheme, so
/// they are left out of both. Nothing is lost: the marketplace and the ACP
/// registry are fetched by the app itself, through the decorator that does see
/// paths.
struct AllowlistPolicy {
    policy: EgressPolicyHandle,
}

impl DestinationPolicy for AllowlistPolicy {
    fn evaluate(&self, host: &str, port: u16) -> PolicyDecision {
        let Some(request_host) = RequestHost::parse(host) else {
            log::warn!(
                "egress proxy: denied a child process's request to {host:?}:{port} (unusable host)"
            );
            return deny(host);
        };
        if self.policy.allows(&request_host) {
            // Loopback and IP literals are legitimate destinations under this
            // fork's allowlist, so `http_proxy`'s DNS-rebinding filter — which
            // exists for the sandbox, where they never are — is waived. The
            // trust level here is the app's own: the same hosts the app may
            // reach, no more.
            return PolicyDecision::Allow {
                permit_local_addresses: true,
            };
        }
        log::warn!(
            "egress proxy: denied a child process's connection to {host}:{port} \
             — not on the network allowlist"
        );
        deny(host)
    }
}

fn deny(host: &str) -> PolicyDecision {
    PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
        host: host.to_string(),
        explanation: format!("host '{host}' is not on the network allowlist"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{EgressPolicy, HostPattern};
    use std::io::{Read as _, Write as _};
    use std::net::{Ipv4Addr, TcpListener, TcpStream};
    use std::time::Duration;

    fn policy_for(hosts: &[&str]) -> AllowlistPolicy {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts);
        assert!(errors.is_empty(), "test hosts should all parse: {errors:?}");
        AllowlistPolicy {
            policy: EgressPolicyHandle::new(policy),
        }
    }

    #[track_caller]
    fn allows(policy: &AllowlistPolicy, host: &str) -> bool {
        match policy.evaluate(host, 443) {
            PolicyDecision::Allow {
                permit_local_addresses,
            } => {
                assert!(
                    permit_local_addresses,
                    "this fork's allowlist decides loopback itself; the rebinding filter \
                     must stay waived"
                );
                true
            }
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy { host: denied, .. }) => {
                assert_eq!(denied, host);
                false
            }
            PolicyDecision::Deny(other) => panic!("unexpected deny reason: {other:?}"),
        }
    }

    #[test]
    fn allows_exactly_what_the_users_allowlist_does() {
        let policy = policy_for(&["api.anthropic.com", "*.corp.example", "10.0.0.5"]);
        assert!(allows(&policy, "api.anthropic.com"));
        assert!(allows(&policy, "git.corp.example"));
        assert!(allows(&policy, "10.0.0.5"));
        assert!(!allows(&policy, "api.openai.com"));
        // A subdomain wildcard does not cover its bare parent, here as
        // everywhere else in this fork.
        assert!(!allows(&policy, "corp.example"));
    }

    #[test]
    fn loopback_needs_no_entry() {
        let policy = policy_for(&[]);
        for host in ["localhost", "127.0.0.1", "127.0.0.2", "::1"] {
            assert!(allows(&policy, host), "{host} should be exempt");
        }
        // Not loopback despite the name — resolvers are not required to keep
        // `*.localhost` local, so it is not exempt.
        assert!(!allows(&policy, "evil.localhost"));
    }

    #[test]
    fn the_path_scoped_built_in_defaults_do_not_apply() {
        // `api.zed.dev/extensions` is opened for the app itself, which fetches
        // it through the decorator that can see paths. A CONNECT tunnel cannot,
        // so a child process gets no such grant.
        let policy = policy_for(&[]);
        assert!(!allows(&policy, "api.zed.dev"));
        assert!(!allows(&policy, "cdn.agentclientprotocol.com"));
    }

    #[test]
    fn an_unparsable_host_is_denied_rather_than_waved_through() {
        let policy = policy_for(&["api.anthropic.com"]);
        for host in ["", " ", "foo..bar"] {
            assert!(
                matches!(
                    policy.evaluate(host, 443),
                    PolicyDecision::Deny(DenyReason::NotPermittedByPolicy { .. })
                ),
                "{host:?} should be denied"
            );
        }
    }

    #[test]
    fn live_policy_changes_are_picked_up_without_restarting_the_proxy() {
        let handle = EgressPolicyHandle::default();
        let policy = AllowlistPolicy {
            policy: handle.clone(),
        };
        assert!(!allows(&policy, "api.anthropic.com"));

        handle.set(EgressPolicy::from_patterns([HostPattern::parse(
            "api.anthropic.com",
        )
        .expect("valid pattern")]));
        assert!(allows(&policy, "api.anthropic.com"));
    }

    /// The whole loop over real sockets: the supervisor binds a listener,
    /// publishes it for the spawn paths, forwards a request to a permitted
    /// destination, refuses one to an unlisted host, and tears everything down
    /// when the feature is turned off.
    ///
    /// The only test here that touches the process-wide endpoint slot, and it
    /// leaves it as it found it.
    #[test]
    fn supervises_a_real_listener_end_to_end() {
        const TIMEOUT: Duration = Duration::from_secs(5);

        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stub origin");
        let origin_port = origin.local_addr().expect("origin addr").port();
        let origin_thread = std::thread::spawn(move || {
            let (mut socket, _) = origin.accept().expect("accept");
            socket
                .set_read_timeout(Some(TIMEOUT))
                .expect("read timeout");
            let mut seen = Vec::new();
            let mut buffer = [0u8; 1024];
            while let Ok(read) = socket.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                seen.extend_from_slice(&buffer[..read]);
                if seen.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("respond");
            socket
                .shutdown(std::net::Shutdown::Write)
                .expect("half-close");
            seen
        });

        // An empty allowlist: the origin is reachable only because it is on
        // loopback, which needs no entry.
        let policy = EgressPolicyHandle::default();
        let mut supervisor = Supervisor::new(None);
        supervisor.apply(true, &policy);

        let endpoint = http_client::child_process_proxy().expect("the proxy should be published");
        let proxy_port = endpoint.port().expect("the endpoint carries a port");

        let mut client =
            TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).expect("connect to the proxy");
        client
            .set_read_timeout(Some(TIMEOUT))
            .expect("read timeout");
        client
            .write_all(
                format!(
                    "GET http://localhost:{origin_port}/ HTTP/1.1\r\n\
                     Host: localhost:{origin_port}\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("send the request");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read response");
        assert!(response.contains("HTTP/1.1 200"), "client got: {response}");

        let origin_saw = origin_thread.join().expect("origin thread");
        assert!(
            String::from_utf8_lossy(&origin_saw).starts_with("GET / HTTP/1.1\r\n"),
            "origin saw: {:?}",
            String::from_utf8_lossy(&origin_saw)
        );

        let mut denied =
            TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).expect("connect to the proxy");
        denied
            .set_read_timeout(Some(TIMEOUT))
            .expect("read timeout");
        denied
            .write_all(b"CONNECT api.openai.com:443 HTTP/1.1\r\nHost: api.openai.com:443\r\n\r\n")
            .expect("send CONNECT");
        let mut response = String::new();
        denied.read_to_string(&mut response).expect("read response");
        assert!(
            response.starts_with("HTTP/1.1 511 "),
            "an unlisted host must be refused, got: {response}"
        );
        assert!(response.contains("api.openai.com"), "{response}");

        supervisor.apply(false, &policy);
        assert_eq!(
            http_client::child_process_proxy(),
            None,
            "turning the feature off must withdraw the endpoint"
        );
        assert!(
            TcpStream::connect_timeout(
                &(Ipv4Addr::LOCALHOST, proxy_port).into(),
                Duration::from_millis(500),
            )
            .is_err(),
            "turning the feature off must stop the listener"
        );

        // Dropping the supervisor is the other way the listener goes away —
        // how `remote_server` tears it down when the project it belongs to is
        // released — and it has to withdraw the endpoint just the same.
        supervisor.apply(true, &policy);
        assert!(
            http_client::child_process_proxy().is_some(),
            "turning the feature back on must publish an endpoint again"
        );
        drop(supervisor);
        assert_eq!(
            http_client::child_process_proxy(),
            None,
            "dropping the supervisor must withdraw the endpoint"
        );
    }
}
