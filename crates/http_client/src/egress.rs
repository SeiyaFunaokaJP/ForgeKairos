//! Deny-by-default egress allowlist. See `docs/jp-enhance/DESIGN.md` §4 (ADR-F03)
//! and §4.38 (ADR-F38) for the two lanes.
//!
//! [`AllowlistedHttpClient`] decorates the process-wide [`HttpClient`] at the
//! single construction point in `crates/zed/src/main.rs`, so every HTTP(S)
//! request the app makes passes through here. What it *gates* is the AI lane
//! (see [`EgressLane`]): LLM providers, agent tools, MCP servers and the
//! external agent processes the fork installs. Requests the IDE's own tooling
//! makes — language servers, formatters, debug adapters, extensions, updates,
//! the remote server — are stamped [`EgressLane::Ide`] at the point they are
//! handed their client and pass through unchecked.
//!
//! Loopback is always allowed so local model servers (Ollama, LM Studio,
//! llama.cpp) work with an empty policy; every other host on the AI lane must
//! be listed — except the few path-scoped entries in [`BUILTIN_ALLOWLIST`].
//!
//! The decorator also follows redirects itself, one hop at a time, because a
//! transport that follows them internally (which `reqwest` does) would move
//! between hosts without this check ever seeing the later hops. It keeps doing
//! that on both lanes, so the credential-stripping rules a cross-host redirect
//! goes through do not depend on which lane the request is on.

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, bail, ensure};
use futures::AsyncRead;
use futures::future::BoxFuture;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use parking_lot::RwLock;
use url::{Host, Url};

use crate::{AsyncBody, HttpClient, HttpClientWithUrl, Inner, RedirectPolicy, RequestTimeout};

/// Which kind of traffic a request is, and therefore whether the allowlist
/// applies to it (`docs/jp-enhance/DESIGN.md` ADR-F38).
///
/// The allowlist exists to keep *the AI* from talking to whatever it likes —
/// a model provider, an agent's fetch/web-search tool, an MCP server, an
/// external agent process. It was never meant to stand between the editor and
/// the tools a developer expects to just work (language servers, formatters,
/// debug adapters, extensions, updates), and gating those made the IDE look
/// broken for reasons that had nothing to do with AI.
///
/// Carried as a request extension rather than as separate client types so that
/// a caller reached through `Arc<dyn HttpClient>` — which is how every one of
/// them is reached — needs no signature change, and so the stamp survives the
/// `HttpClientWithUrl` / `HttpClientWithProxy` wrappers, which pass extensions
/// through untouched.
///
/// [`Ai`](Self::Ai) is the default for a request that carries no stamp at all,
/// which keeps the system fail-closed: a new call site is gated until someone
/// decides it is IDE infrastructure and says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressLane {
    /// Gated by the allowlist. Anything the model or an agent can steer.
    Ai,
    /// Not gated. The editor's own tooling, acting for the user.
    Ide,
}

/// Stamps every request it forwards with an [`EgressLane`], leaving the rest of
/// the request untouched. Wraps *outside* [`AllowlistedHttpClient`] (usually
/// far outside — at the point some subsystem is handed its client), because the
/// decorator has to see the stamp to act on it.
pub struct LaneHttpClient {
    inner: Arc<dyn HttpClient>,
    lane: EgressLane,
}

impl LaneHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>, lane: EgressLane) -> Self {
        Self { inner, lane }
    }
}

impl HttpClient for LaneHttpClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(
        &self,
        mut request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        // A caller that stamped a lane itself keeps it: the only such caller is
        // another `LaneHttpClient`, and the innermost one is the one that knows
        // what the request really is.
        if request.extensions().get::<EgressLane>().is_none() {
            request.extensions_mut().insert(self.lane);
        }
        self.inner.send(request)
    }

    fn egress_policy(&self) -> Option<&EgressPolicyHandle> {
        self.inner.egress_policy()
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &crate::FakeHttpClient {
        self.inner.as_fake()
    }
}

/// Marks everything sent through `inner` as IDE tooling traffic, exempt from
/// the allowlist.
pub fn ide_lane(inner: Arc<dyn HttpClient>) -> Arc<dyn HttpClient> {
    Arc::new(LaneHttpClient::new(inner, EgressLane::Ide))
}

/// [`ide_lane`] for the callers that need the URL-building surface of
/// [`HttpClientWithUrl`] (the extension store builds every marketplace URL with
/// it). Shares the base URL cell rather than copying the string, so a later
/// `set_base_url` is still seen through this handle.
pub fn ide_lane_with_url(inner: Arc<HttpClientWithUrl>) -> Arc<HttpClientWithUrl> {
    let lane = ide_lane(inner.clone());
    Arc::new(inner.with_transport(lane))
}

/// Cap applied to [`RedirectPolicy::FollowAll`]. Matches the limit
/// `reqwest_client` used to hand to reqwest for that policy.
const FOLLOW_ALL_LIMIT: u32 = 100;

/// Applied when a request carries no [`RedirectPolicy`] extension at all.
/// `reqwest::ClientBuilder`'s own default follows up to 10 redirects, and it
/// does so inside reqwest where this decorator never sees the hops. Taking
/// those hops over here keeps the observable behavior of such callers while
/// closing the hole.
const DEFAULT_REDIRECT_LIMIT: u32 = 10;

/// Headers that must not be carried to a different host after a redirect.
/// `authorization` / `cookie` are what `reqwest` strips; the API-key headers
/// are added because this fork's whole point is that provider credentials stay
/// where they were meant to go.
const CROSS_HOST_STRIPPED_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "proxy-authorization",
    "www-authenticate",
    "x-api-key",
    "api-key",
    // A caller-supplied `Host` would no longer describe the target.
    "host",
];

/// Headers describing a request body, dropped when a redirect turns the
/// request into a bodyless `GET`.
const BODY_HEADERS: &[&str] = &[
    "content-length",
    "content-type",
    "content-encoding",
    "transfer-encoding",
];

/// A host as it appears in a request URL, canonicalized for matching:
/// lowercase, IDN in Punycode (A-label) form, no trailing dot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestHost {
    Domain(String),
    Ip(IpAddr),
}

impl RequestHost {
    /// Canonicalize a raw host string (`Host:` header / URL authority form).
    /// IPv6 literals may be bracketed or bare.
    pub fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        let unbracketed = raw
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(raw);
        if let Ok(ip) = unbracketed.parse::<IpAddr>() {
            return Some(RequestHost::Ip(ip));
        }
        match Host::parse(raw).ok()? {
            Host::Domain(domain) => Some(RequestHost::Domain(canonical_domain(&domain))),
            Host::Ipv4(ip) => Some(RequestHost::Ip(IpAddr::V4(ip))),
            Host::Ipv6(ip) => Some(RequestHost::Ip(IpAddr::V6(ip))),
        }
    }

    /// `url::Url` has already lowercased and Punycoded the host at parse time,
    /// so this avoids re-parsing it.
    fn from_url_host(host: Host<&str>) -> Self {
        match host {
            Host::Domain(domain) => RequestHost::Domain(canonical_domain(domain)),
            Host::Ipv4(ip) => RequestHost::Ip(IpAddr::V4(ip)),
            Host::Ipv6(ip) => RequestHost::Ip(IpAddr::V6(ip)),
        }
    }

    /// Hosts that always resolve to this machine and are therefore exempt from
    /// the allowlist. Deliberately narrow: the loopback IP ranges and the bare
    /// name `localhost`. `*.localhost` is *not* included — resolvers are not
    /// required to keep it on loopback, so it would be a hole rather than an
    /// exemption.
    pub fn is_loopback(&self) -> bool {
        match self {
            RequestHost::Ip(IpAddr::V4(ip)) => ip.is_loopback(),
            // `::ffff:127.0.0.1` really is loopback; `Ipv6Addr::is_loopback`
            // only recognizes `::1`.
            RequestHost::Ip(IpAddr::V6(ip)) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
            RequestHost::Domain(domain) => domain == "localhost",
        }
    }
}

/// Whether a configured endpoint (such as a self-hosted model server's
/// `api_url`) is on this machine. Anything that does not parse is not.
///
/// zed-jp-enhance: shared by the surfaces that may reach a self-hosted AI
/// endpoint without the external-AI consent only while it is local (ADR-F10).
/// The host is read exactly as [`EgressPolicyHandle::check_url`] reads it, so
/// this never calls local what the allowlist would treat as remote.
pub fn endpoint_is_loopback(api_url: &str) -> bool {
    Url::parse(api_url)
        .ok()
        .and_then(|url| url.host().map(RequestHost::from_url_host))
        .is_some_and(|host| host.is_loopback())
}

impl fmt::Display for RequestHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestHost::Domain(domain) => write!(f, "{domain}"),
            RequestHost::Ip(IpAddr::V4(ip)) => write!(f, "{ip}"),
            RequestHost::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]"),
        }
    }
}

fn canonical_domain(domain: &str) -> String {
    // Hostnames are case-insensitive (RFC 1035 §2.3.3) and trailing-dot
    // tolerant: `example.com.` and `example.com` are the same host.
    domain
        .strip_suffix('.')
        .unwrap_or(domain)
        .to_ascii_lowercase()
}

/// One entry of the allowlist.
///
/// Same shape as `http_proxy::HostPattern` (exact host, or a leading `*.`
/// subdomain wildcard that does *not* match the bare parent), plus IP
/// literals, which that type rejects by design. Reusing it directly was
/// rejected for that reason and because `http_client` also builds for wasm,
/// where `http_proxy`'s dependencies do not belong. See DESIGN.md §4.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostPattern {
    /// Matches exactly one hostname.
    Exact(String),
    /// Matches subdomains of the contained hostname: `*.example.com` matches
    /// `a.example.com` and `a.b.example.com`, but not `example.com` itself.
    Subdomain(String),
    /// Matches one IP literal exactly.
    Ip(IpAddr),
}

/// Why a policy-file entry was rejected. Rejected entries are skipped, never
/// fatal, so one typo cannot take the whole allowlist down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPatternError {
    pub pattern: String,
    pub reason: String,
}

impl fmt::Display for HostPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid host pattern {:?}: {}",
            self.pattern, self.reason
        )
    }
}

impl std::error::Error for HostPatternError {}

impl HostPattern {
    pub fn parse(input: &str) -> Result<Self, HostPatternError> {
        let reject = |reason: &str| HostPatternError {
            pattern: input.to_string(),
            reason: reason.to_string(),
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(reject("pattern is empty"));
        }

        let (is_subdomain, rest) = match trimmed.strip_prefix("*.") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        if rest.contains('*') {
            return Err(reject("`*` is only allowed as the leading `*.` label"));
        }

        let rest = rest.strip_suffix('.').unwrap_or(rest);
        if rest.is_empty() {
            return Err(reject("pattern is empty"));
        }

        match RequestHost::parse(rest) {
            Some(RequestHost::Ip(ip)) => {
                if is_subdomain {
                    return Err(reject("`*.` cannot be applied to an IP literal"));
                }
                Ok(HostPattern::Ip(ip))
            }
            Some(RequestHost::Domain(domain)) => {
                if domain.starts_with('.') || domain.contains("..") {
                    return Err(reject("hostname has an empty label"));
                }
                Ok(if is_subdomain {
                    HostPattern::Subdomain(domain)
                } else {
                    HostPattern::Exact(domain)
                })
            }
            None => Err(reject("not a valid hostname or IP literal")),
        }
    }

    pub fn matches(&self, host: &RequestHost) -> bool {
        match (self, host) {
            (HostPattern::Exact(pattern), RequestHost::Domain(domain)) => pattern == domain,
            (HostPattern::Subdomain(parent), RequestHost::Domain(domain)) => domain
                .strip_suffix(parent.as_str())
                .is_some_and(|label| label.ends_with('.') && label.len() > 1),
            (HostPattern::Ip(pattern), RequestHost::Ip(ip)) => pattern == ip,
            _ => false,
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostPattern::Exact(host) => write!(f, "{host}"),
            HostPattern::Subdomain(parent) => write!(f, "*.{parent}"),
            HostPattern::Ip(IpAddr::V4(ip)) => write!(f, "{ip}"),
            HostPattern::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]"),
        }
    }
}

/// The set of hosts this process may reach. Empty means "loopback only".
#[derive(Debug, Clone, Default)]
pub struct EgressPolicy {
    patterns: Vec<HostPattern>,
}

impl EgressPolicy {
    /// Allows nothing but loopback. Also what [`Default`] produces, which is
    /// what the app runs on before the policy file has been read.
    pub fn deny_all() -> Self {
        Self::default()
    }

    pub fn from_patterns(patterns: impl IntoIterator<Item = HostPattern>) -> Self {
        Self {
            patterns: patterns.into_iter().collect(),
        }
    }

    /// Parses user-supplied host strings, keeping the valid ones and returning
    /// one error per rejected entry for the caller to report.
    pub fn from_host_strings<S: AsRef<str>>(
        hosts: impl IntoIterator<Item = S>,
    ) -> (Self, Vec<HostPatternError>) {
        let mut patterns = Vec::new();
        let mut errors = Vec::new();
        for host in hosts {
            match HostPattern::parse(host.as_ref()) {
                Ok(pattern) => patterns.push(pattern),
                Err(error) => errors.push(error),
            }
        }
        (Self { patterns }, errors)
    }

    pub fn patterns(&self) -> &[HostPattern] {
        &self.patterns
    }

    pub fn allows(&self, host: &RequestHost) -> bool {
        host.is_loopback() || self.patterns.iter().any(|pattern| pattern.matches(host))
    }
}

/// A destination this fork opens without the user having to list it, together
/// with the paths on it that are opened.
///
/// The path scoping is the point. The extension marketplace shares
/// `api.zed.dev` with telemetry (`/telemetry/events`), which this fork ships
/// disabled and deliberately does not want re-enabled as a side effect of
/// making extensions work. Host granularity — what the user's allowlist offers
/// — cannot express that, so this table carries prefixes as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinAllow {
    /// What this entry exists for. Named in the log when it lets a request
    /// through, so a built-in hit is distinguishable from a user-listed one.
    pub purpose: &'static str,
    /// A host pattern in the same syntax as the user's allowlist.
    pub host: &'static str,
    /// Allowed path prefixes. A prefix matches the whole path or a path whose
    /// next character is `/`, so `/extensions` covers `/extensions/x/download`
    /// but not `/extensions-private`.
    pub paths: &'static [&'static str],
}

impl BuiltinAllow {
    fn matches_path(&self, path: &str) -> bool {
        self.paths.iter().any(|prefix| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }
}

/// Purpose string shared by the extension marketplace entries.
const EXTENSION_MARKETPLACE: &str = "extension marketplace";

/// Purpose string shared by the ACP agent registry entries.
const ACP_REGISTRY: &str = "ACP agent registry";

/// Purpose string for this fork's own update check.
const APP_UPDATE_CHECK: &str = "app update check";

/// The GitHub repository this fork updates itself from (ADR-F16). It lives
/// here rather than in `auto_update` because the built-in entry below has to
/// scope the update check to exactly this repository's releases, and
/// `auto_update` already depends on this crate.
pub const UPDATE_REPO: &str = branding::UPDATE_REPO;

/// The GitHub API path prefix the update check reads.
const UPDATE_RELEASES_PATH: &str = branding::concat_const!("/repos/", UPDATE_REPO, "/releases");

/// Where a `github.com` release asset URL redirects to. GitHub answers the
/// `…/releases/download/…` URL with a 302 to a pre-signed URL on one of these
/// hosts, and this decorator re-checks every hop, so opening only `github.com`
/// gets the download refused one hop later.
///
/// Both are listed because GitHub does not document which one a given release
/// answers with and has changed it before: a 2026-08-30 measurement of a real
/// release asset redirect landed on `release-assets.githubusercontent.com`,
/// while `objects.githubusercontent.com` is the host every earlier observation
/// (and the rest of the ecosystem's allowlists) recorded. Consenting to a
/// download opens both, which costs nothing that consenting to the newer host
/// alone does not already cost — they are two names for the same release-asset
/// CDN — and saves the user a second denial when GitHub picks the other one.
const GITHUB_RELEASE_ASSET_HOSTS: &[&str] = &[
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// The hosts that have to be on the allowlist for a download from `url` to
/// run to completion — its own host, plus any host it is known to redirect
/// to.
///
/// Consent flows that ask the user to open a download host must ask for the
/// whole set at once: asking for the first host only means the user says yes
/// and the download still fails on the next hop. Returns an empty set for a
/// URL with no host, which callers should read as "cannot tell", not "nothing
/// needed".
///
/// The set is a best effort — a redirect to somewhere not listed here is
/// still refused by [`EgressPolicyHandle::check_url`], naming the host it
/// blocked, so a caller can recover from the [`EgressDenied`] error too.
pub fn hosts_required_to_download(url: &str) -> Vec<String> {
    let Some(host) = Url::parse(url)
        .ok()
        .and_then(|url| url.host().map(RequestHost::from_url_host))
    else {
        return Vec::new();
    };
    hosts_required_after(&host.to_string())
}

/// The hosts a download starting at `host` needs opened, `host` included.
///
/// Split out of [`hosts_required_to_download`] so that a consent flow which
/// only has the *denied* host — not the URL that was going to be fetched —
/// asks for the same set. `host` must already be canonicalized the way
/// [`RequestHost`] spells it (lowercased, no trailing dot); both callers here
/// pass one that is.
fn hosts_required_after(host: &str) -> Vec<String> {
    match host {
        // A release listing is never the last request: the caller reads an
        // asset URL out of it and fetches that from `github.com`, which
        // redirects again. Asking for the API host alone buys one more denial.
        "api.github.com" => {
            let mut hosts = vec![host.to_string(), "github.com".to_string()];
            hosts.extend(GITHUB_RELEASE_ASSET_HOSTS.iter().map(|it| it.to_string()));
            hosts
        }
        "github.com" => {
            let mut hosts = vec![host.to_string()];
            hosts.extend(GITHUB_RELEASE_ASSET_HOSTS.iter().map(|it| it.to_string()));
            hosts
        }
        _ => vec![host.to_string()],
    }
}

/// The hosts to ask the user for when `error` — or anything it wraps — is an
/// [`EgressDenied`]: the host that was refused, plus the hosts that same
/// operation is known to need next (see [`hosts_required_after`]).
///
/// `None` when nothing in the chain was an egress denial, which callers must
/// keep distinct from "denied, but nothing to ask for": only the former means
/// the failure had some other cause and no consent prompt applies.
///
/// Walks the chain rather than downcasting the outermost error because every
/// layer between the decorator and the caller adds its own `context`.
pub fn egress_denied_hosts(error: &anyhow::Error) -> Option<Vec<String>> {
    let denied = error
        .chain()
        .find_map(|error| error.downcast_ref::<EgressDenied>())?;
    // The host in `EgressDenied` came out of a parsed `Url`, but a policy that
    // refuses a host it could not parse would still name it, so canonicalize
    // once more instead of trusting the shape.
    let host = RequestHost::parse(&denied.host)
        .map(|host| host.to_string())
        .unwrap_or_else(|| denied.host.to_ascii_lowercase());
    Some(hosts_required_after(&host))
}

/// Destinations allowed regardless of the user's allowlist (2026-08-28 product
/// decision: extension search and install must work out of the box).
///
/// ADR-F38 narrowed what the allowlist covers to the AI lane, which left only
/// the ACP registry entry load-bearing — the marketplace, the bucket it
/// redirects to and this fork's release index are all reached by tooling that
/// is no longer gated at all. The table is kept whole anyway: those three are
/// path-scoped GETs on destinations this fork already trusts, and keeping them
/// means a request that arrives on the AI lane because some path was never
/// stamped still works instead of failing in a way nobody would connect to
/// this table.
///
/// Scope is deliberately just what browsing and installing an extension needs:
/// `ExtensionStore` builds every marketplace URL with
/// `HttpClientWithUrl::build_zed_api_url("/extensions…")`, which resolves to
/// `api.zed.dev` for the default `server_url`, and the `…/download` endpoints
/// answer 307 with a pre-signed URL on Zed's object storage bucket. Because
/// this decorator re-checks every redirect hop, that bucket has to be listed
/// too. Nothing else is: telemetry, auto-update and sign-in are out by
/// construction (different paths or different hosts), and the toolchain
/// downloads a *dev* extension build needs (wasi-sdk from GitHub) are a
/// developer flow, not the marketplace.
///
/// The ACP registry entry below is a second, narrower instance of the same
/// idea (2026-08-28, F2-5): the fork installs external agent CLIs through an
/// IDE-driven notify-and-button flow rather than bundling them (see
/// `docs/jp-enhance/AGENTS-SETUP.md` "方針"), and *checking whether one exists or
/// has a newer version* has to work with an empty allowlist, the same as
/// browsing extensions does — only the follow-up download of the agent
/// binary itself goes through the F2-5 consent flow, which adds the vendor's
/// own host (`github.com`, `dl.google.com`, …) to the user's allowlist.
/// `crates/project/src/agent_registry_store.rs` fetches
/// `REGISTRY_URL = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json"`
/// and, for every agent, an icon at
/// `https://cdn.agentclientprotocol.com/registry/v1/latest/{id}.svg` (a raw
/// fetch of the live registry on 2026-08-28 confirmed all 39 listed agents'
/// `icon` fields point there — none fall back to the `raw.githubusercontent.com`
/// path the code also handles, so that fallback is deliberately left off this
/// table). Every agent's actual archive/binary URL — the thing this table
/// does *not* open — lives on a distribution-specific host entirely disjoint
/// from `cdn.agentclientprotocol.com` (observed hosts as of the same date:
/// `github.com`, `dl.google.com`, `downloads.cursor.com`,
/// `downloads.poolside.ai`, `sfc-repo.snowflakecomputing.com`,
/// `static.devin.ai`).
///
/// The update-check entry (2026-08-30, ADR-F16) applies the same rule once
/// more: this fork updates itself from its *own* GitHub Releases, and asking
/// "is there a newer build?" has to work with an empty allowlist, the same as
/// browsing extensions does. Only the release index of that one repository is
/// opened. The binaries themselves live on `github.com` (redirecting to
/// [`GITHUB_RELEASE_ASSET_HOSTS`]), which are deliberately left out: those
/// hosts carry the whole of GitHub's download surface, so *downloading* an
/// update goes through the same explicit consent step an agent install does.
pub const BUILTIN_ALLOWLIST: &[BuiltinAllow] = &[
    // Listing, search, version lookup, update check and the download redirect.
    BuiltinAllow {
        purpose: EXTENSION_MARKETPLACE,
        host: "api.zed.dev",
        paths: &["/extensions"],
    },
    // Where `/extensions/{id}/download` redirects: the extension archives
    // themselves. Observed to be this exact host for every extension tried, so
    // it is listed exactly rather than as a wildcard — `*.digitaloceanspaces.com`
    // would hand every other tenant's bucket an exfiltration channel.
    BuiltinAllow {
        purpose: EXTENSION_MARKETPLACE,
        host: "zed-extensions.nyc3.digitaloceanspaces.com",
        paths: &["/extensions"],
    },
    // The ACP registry index (used both for listing agents and for detecting
    // a newer version of one already installed) and every agent's icon —
    // both under the same `/registry/v1` prefix on the same host, so one
    // entry covers both without opening anything else on it.
    BuiltinAllow {
        purpose: ACP_REGISTRY,
        host: "cdn.agentclientprotocol.com",
        paths: &["/registry/v1"],
    },
    // This fork's own release index: `/releases`, `/releases/latest` and
    // `/releases/tags/{tag}` under exactly one repository. Nothing else on
    // `api.github.com` — extensions and language servers that read other
    // repositories still need the user to list the host — and not the
    // download hosts, which the consent flow adds.
    BuiltinAllow {
        purpose: APP_UPDATE_CHECK,
        host: "api.github.com",
        paths: &[UPDATE_RELEASES_PATH],
    },
];

/// [`BUILTIN_ALLOWLIST`] with its host strings parsed once. An entry that fails
/// to parse is a bug in the table above; it is dropped (and logged) rather than
/// guessed at, so a typo can only close the network down, never open it.
static BUILTIN_PATTERNS: LazyLock<Vec<(HostPattern, &'static BuiltinAllow)>> =
    LazyLock::new(|| {
        BUILTIN_ALLOWLIST
            .iter()
            .filter_map(|entry| match HostPattern::parse(entry.host) {
                Ok(pattern) => Some((pattern, entry)),
                Err(error) => {
                    log::error!("egress allowlist: dropping built-in entry — {error}");
                    None
                }
            })
            .collect()
    });

/// The built-in entry that permits this host and path, if any.
fn builtin_allow_for(host: &RequestHost, path: &str) -> Option<&'static BuiltinAllow> {
    BUILTIN_PATTERNS
        .iter()
        .find(|(pattern, entry)| pattern.matches(host) && entry.matches_path(path))
        .map(|(_, entry)| *entry)
}

/// Shared, live-updatable handle to the current [`EgressPolicy`]. Cloning is
/// `Arc`-cheap; the allow path takes a read lock and runs the host match.
#[derive(Clone, Default)]
pub struct EgressPolicyHandle {
    policy: Arc<RwLock<EgressPolicy>>,
    subscribers: Arc<RwLock<Vec<futures::channel::mpsc::UnboundedSender<()>>>>,
}

impl EgressPolicyHandle {
    pub fn new(policy: EgressPolicy) -> Self {
        Self {
            policy: Arc::new(RwLock::new(policy)),
            subscribers: Arc::default(),
        }
    }

    /// Replaces the policy. Takes effect on the next request; requests already
    /// in flight are not retroactively cancelled.
    pub fn set(&self, policy: EgressPolicy) {
        *self.policy.write() = policy;
        // Woken after the policy is in place, so a subscriber that reads the
        // handle from its callback can never see the previous one.
        self.subscribers
            .write()
            .retain(|subscriber| subscriber.unbounded_send(()).is_ok());
    }

    /// A stream that yields once per [`Self::set`], for code holding a copy of
    /// the allowlist somewhere the handle cannot reach — a `remote_server` on
    /// the far side of an SSH connection, which was handed the list once at
    /// connect time (ADR-F11) and has to be re-sent it when the user consents
    /// to a new host mid-session (ADR-F22).
    ///
    /// Unbounded and lossy in the useful direction: a subscriber that falls
    /// behind sees every edge, and one that has gone away is dropped from the
    /// list on the next `set`.
    pub fn subscribe(&self) -> futures::channel::mpsc::UnboundedReceiver<()> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        self.subscribers.write().push(tx);
        rx
    }

    pub fn snapshot(&self) -> EgressPolicy {
        self.policy.read().clone()
    }

    /// Whether the *user's* policy (plus loopback) permits this host. The
    /// built-in defaults are path-scoped, so they cannot be decided from a host
    /// alone and are only consulted by [`Self::check_url`].
    pub fn allows(&self, host: &RequestHost) -> bool {
        self.policy.read().allows(host)
    }

    /// Checks one URL against the built-in defaults unioned with the user's
    /// policy, producing the error surfaced to callers when denied.
    pub fn check_url(&self, url: &Url) -> Result<(), EgressDenied> {
        let host = url
            .host()
            .map(RequestHost::from_url_host)
            .ok_or_else(|| EgressDenied {
                host: "<none>".to_string(),
                url: redacted_url(url),
            })?;
        if self.allows(&host) {
            return Ok(());
        }
        // `Url` has already resolved `.` / `..` segments, so a path prefix
        // cannot be walked out of.
        if let Some(entry) = builtin_allow_for(&host, url.path()) {
            log::debug!(
                "egress allowlist: allowing {} by the built-in {} rule",
                redacted_url(url),
                entry.purpose
            );
            return Ok(());
        }
        Err(EgressDenied {
            host: host.to_string(),
            url: redacted_url(url),
        })
    }
}

/// Proxy environment variables, in the casings tools actually read. Both
/// casings of each name are set together so a tool that reads only one of them
/// still sees the proxy, and the presence of *any* of them is what marks a
/// child process as already having a proxy configured by the user.
const CHILD_PROCESS_PROXY_VARS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Destinations a proxied child process should reach directly. Matches the
/// loopback exemption [`RequestHost::is_loopback`] applies to the app's own
/// requests, so a local model server stays reachable even if the proxy is
/// unavailable.
const CHILD_PROCESS_NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// Endpoint of the loopback forward proxy this fork points agent-harness child
/// processes at — ACP agents, MCP servers and npm (`docs/jp-enhance/DESIGN.md`
/// ADR-F13). `None` until `egress_policy` has a listener up, and again if the
/// user turns the feature off.
///
/// A process-wide slot rather than a field on [`AllowlistedHttpClient`]:
/// `node_runtime`, one of the three spawn paths that has to read it, holds no
/// `App` and depends on neither `gpui` nor `egress_policy`, while the crate
/// that owns the listener sits above both. This crate is the lowest common
/// dependency of every reader and the one writer.
static CHILD_PROCESS_PROXY: RwLock<Option<Url>> = RwLock::new(None);

/// Publishes (or, with `None`, withdraws) the child-process proxy endpoint.
/// Called only by `egress_policy`, which owns the listener's lifetime.
pub fn set_child_process_proxy(endpoint: Option<Url>) {
    *CHILD_PROCESS_PROXY.write() = endpoint;
}

/// The endpoint child processes should be pointed at, if one is running.
pub fn child_process_proxy() -> Option<Url> {
    CHILD_PROCESS_PROXY.read().clone()
}

/// Points a child process's environment at [`child_process_proxy`], and
/// reports whether it did.
///
/// Does nothing when the proxy is not running, and — deliberately — nothing at
/// all when `env` already carries any proxy variable. A user who spelled out a
/// proxy for an agent in `agent_servers.<name>.env` (or for an MCP server in
/// `context_servers.<name>.env`) means it, and half-overwriting their
/// configuration would leave the child with two contradictory proxies rather
/// than either one.
pub fn apply_child_process_proxy_env<S: BuildHasher>(env: &mut HashMap<String, String, S>) -> bool {
    match child_process_proxy() {
        Some(endpoint) => apply_proxy_env(env, &endpoint),
        None => false,
    }
}

/// Whether `env` already spells out a proxy of its own, which is exactly the
/// condition [`apply_child_process_proxy_env`] refuses to overwrite. Exposed so
/// UI that reports whether a child process is actually proxied can recompute the
/// decision — the injection leaves no trace on the stored configuration.
pub fn env_overrides_child_process_proxy<S: BuildHasher>(env: &HashMap<String, String, S>) -> bool {
    existing_proxy_var(env).is_some()
}

fn existing_proxy_var<S: BuildHasher>(env: &HashMap<String, String, S>) -> Option<&'static str> {
    CHILD_PROCESS_PROXY_VARS
        .iter()
        .copied()
        .find(|name| env.contains_key(*name))
}

/// Generic over the hasher because the spawn paths do not agree on one:
/// `agent_servers` and `context_server` carry `collections::HashMap` (Fx),
/// `node_runtime` carries `std::collections::HashMap`.
fn apply_proxy_env<S: BuildHasher>(env: &mut HashMap<String, String, S>, endpoint: &Url) -> bool {
    if let Some(existing) = existing_proxy_var(env) {
        log::debug!(
            "egress proxy: leaving child process alone; its environment already sets {existing}"
        );
        return false;
    }

    let endpoint = endpoint.to_string();
    for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
        env.insert(name.to_string(), endpoint.clone());
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.insert(name.to_string(), CHILD_PROCESS_NO_PROXY.to_string());
    }
    true
}

/// A request refused because its host is not on the allowlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressDenied {
    pub host: String,
    /// The requested URL with its query string removed — query strings carry
    /// API keys, and this string ends up in logs and UI notifications.
    pub url: String,
}

impl fmt::Display for EgressDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "egress blocked: `{}` is not on the network allowlist (requested {}). \
             Add it to `allowed_hosts` in network_allowlist.json to permit it.",
            self.host, self.url
        )
    }
}

impl std::error::Error for EgressDenied {}

/// Opening token of the line [`egress_denied_marker_line`] writes, and the
/// needle [`egress_denied_from_message`] looks for.
///
/// Deliberately not a word that occurs in prose: the decoder searches for it
/// anywhere in a message, because the layer that flattened the error may have
/// wrapped the marked text in its own (`format!("failed to install: {error}")`).
pub const EGRESS_DENIED_MARKER: &str = "zed:egress-denied";

/// Renders an [`EgressDenied`] as one line that survives a boundary which turns
/// errors into plain strings — the extension WASM host (`wasm_host/wit.rs`) and
/// ADR-F27's HTTP relay, both of which lose the error chain and with it every
/// `downcast_ref` a consent flow depends on.
///
/// Written in front of the human-readable text rather than instead of it, so a
/// reader who only sees the string still sees the original error.
///
/// Neither field can contain a space or a newline (a host has none by
/// construction, and a URL that did would not have parsed), so the line is
/// unambiguous to split back apart.
pub fn egress_denied_marker_line(denied: &EgressDenied) -> String {
    format!("{EGRESS_DENIED_MARKER} {} {}", denied.host, denied.url)
}

/// Rebuilds the [`EgressDenied`] that [`egress_denied_marker_line`] wrote into
/// `message`, or `None` when the message carries no marker.
///
/// Callers put the result back on an `anyhow` chain (or hand it straight to
/// [`egress_denied_hosts`]) so a denial that crossed a stringifying boundary is
/// classified exactly like one that did not.
pub fn egress_denied_from_message(message: &str) -> Option<EgressDenied> {
    let start = message.find(EGRESS_DENIED_MARKER)?;
    let line = message[start + EGRESS_DENIED_MARKER.len()..]
        .lines()
        .next()?;
    let mut fields = line.split_whitespace();
    let host = fields.next()?;
    let url = fields.next()?;
    if host.is_empty() || url.is_empty() {
        return None;
    }
    Some(EgressDenied {
        host: host.to_string(),
        url: url.to_string(),
    })
}

/// Scheme, host, port and path only. The query string and any userinfo are
/// dropped because API keys live there and this string reaches the log and the
/// error surfaced to the user.
fn redacted_url(url: &Url) -> String {
    let mut redacted = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        redacted.push(':');
        redacted.push_str(&port.to_string());
    }
    redacted.push_str(url.path());
    redacted
}

/// Wraps another [`HttpClient`], refusing any request whose host is not
/// permitted by the policy and following redirects hop by hop so that every
/// hop is checked the same way.
pub struct AllowlistedHttpClient {
    inner: Arc<dyn HttpClient>,
    policy: EgressPolicyHandle,
}

impl AllowlistedHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>, policy: EgressPolicyHandle) -> Self {
        Self { inner, policy }
    }

    pub fn policy(&self) -> &EgressPolicyHandle {
        &self.policy
    }
}

impl HttpClient for AllowlistedHttpClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        Box::pin(send_allowlisted(
            self.inner.clone(),
            self.policy.clone(),
            req,
        ))
    }

    fn egress_policy(&self) -> Option<&EgressPolicyHandle> {
        Some(&self.policy)
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &crate::FakeHttpClient {
        self.inner.as_fake()
    }
}

/// A request body that can be re-sent on a redirect, when the body allows it.
enum ReplayableBody {
    Empty,
    Bytes(bytes::Bytes),
    /// A streaming body can only be sent once; `None` once it has been.
    Stream(Option<Pin<Box<dyn AsyncRead + Send + Sync>>>),
}

impl ReplayableBody {
    fn new(body: AsyncBody) -> Self {
        match body.0 {
            Inner::Empty => ReplayableBody::Empty,
            Inner::Bytes(cursor) => ReplayableBody::Bytes(cursor.into_inner()),
            Inner::AsyncReader(reader) => ReplayableBody::Stream(Some(reader)),
        }
    }

    /// The body for the next hop, or `None` if it was a one-shot stream that
    /// has already been sent.
    fn next(&mut self) -> Option<AsyncBody> {
        match self {
            ReplayableBody::Empty => Some(AsyncBody::empty()),
            ReplayableBody::Bytes(bytes) => Some(AsyncBody::from_bytes(bytes.clone())),
            ReplayableBody::Stream(reader) => reader
                .take()
                .map(|reader| AsyncBody(Inner::AsyncReader(reader))),
        }
    }
}

async fn send_allowlisted(
    inner: Arc<dyn HttpClient>,
    policy: EgressPolicyHandle,
    request: Request<AsyncBody>,
) -> anyhow::Result<Response<AsyncBody>> {
    let (mut parts, body) = request.into_parts();

    // Unstamped means AI: a call site nobody has classified stays gated
    // (ADR-F38).
    let lane = parts
        .extensions
        .get::<EgressLane>()
        .copied()
        .unwrap_or(EgressLane::Ai);
    let requested_policy = parts.extensions.get::<RedirectPolicy>().cloned();
    let follows_redirects = !matches!(requested_policy, Some(RedirectPolicy::NoFollow));
    let mut hops_left = match requested_policy {
        None => DEFAULT_REDIRECT_LIMIT,
        Some(RedirectPolicy::NoFollow) => 0,
        Some(RedirectPolicy::FollowLimit(limit)) => limit,
        Some(RedirectPolicy::FollowAll) => FOLLOW_ALL_LIMIT,
    };
    // The inner client must never follow a hop itself: reqwest does so inside
    // its own connection handling, where this decorator cannot re-check the
    // host it moved to.
    parts.extensions.insert(RedirectPolicy::NoFollow);
    let timeout = parts.extensions.get::<RequestTimeout>().copied();

    let mut url = Url::parse(&parts.uri.to_string()).with_context(|| {
        format!(
            "egress allowlist: request URI `{}` is not an absolute URL",
            parts.uri
        )
    })?;
    let version = parts.version;
    let mut method = parts.method.clone();
    let mut headers = parts.headers.clone();
    let mut body = ReplayableBody::new(body);
    // The first hop reuses the caller's own parts so that no extension or URI
    // detail is lost; later hops are rebuilt from `method`/`url`/`headers`.
    let mut original_parts = Some(parts);

    loop {
        // The IDE lane skips only the allowlist check; every hop still goes
        // around this same loop, so the redirect limit and the cross-host
        // header stripping below apply identically on both lanes.
        match lane {
            EgressLane::Ai => policy.check_url(&url).inspect_err(|denied| {
                log::warn!("{denied}");
            })?,
            EgressLane::Ide => {
                log::trace!("egress: IDE-lane request to {}", redacted_url(&url));
            }
        }

        let Some(next_body) = body.next() else {
            bail!(
                "egress allowlist: cannot follow a redirect to {url} because the request body is a \
                 one-shot stream that has already been sent"
            );
        };
        let request = match original_parts.take() {
            Some(parts) => Request::from_parts(parts, next_body),
            None => {
                let mut builder = Request::builder()
                    .method(method.clone())
                    .uri(url.as_str())
                    .version(version)
                    .extension(RedirectPolicy::NoFollow);
                if let Some(timeout) = timeout {
                    builder = builder.extension(timeout);
                }
                if let Some(map) = builder.headers_mut() {
                    *map = headers.clone();
                }
                builder.body(next_body)?
            }
        };

        let response = inner.send(request).await?;
        let status = response.status();

        if !follows_redirects {
            return Ok(response);
        }
        let Some(target) = redirect_target(&response, &url)? else {
            return Ok(response);
        };
        ensure!(
            hops_left > 0,
            "egress allowlist: too many redirects while requesting {url}"
        );
        hops_left -= 1;

        // 303 always becomes a bodyless GET; 301/302 do too for POST, which is
        // what every browser and reqwest do in practice.
        let becomes_get = status == StatusCode::SEE_OTHER
            || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
                && method == Method::POST);
        if becomes_get {
            if method != Method::HEAD {
                method = Method::GET;
            }
            body = ReplayableBody::Empty;
            remove_headers(&mut headers, BODY_HEADERS);
        }
        if target.host_str() != url.host_str() {
            remove_headers(&mut headers, CROSS_HOST_STRIPPED_HEADERS);
        }

        drop(response);
        url = target;
    }
}

fn redirect_target(response: &Response<AsyncBody>, base: &Url) -> anyhow::Result<Option<Url>> {
    if !matches!(
        response.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return Ok(None);
    }
    let Some(location) = response.headers().get(header::LOCATION) else {
        return Ok(None);
    };
    let location = location
        .to_str()
        .context("redirect response has an invalid Location header")?;
    let target = base
        .join(location)
        .with_context(|| format!("invalid redirect target {location:?}"))?;
    ensure!(
        matches!(target.scheme(), "http" | "https"),
        "refusing to follow redirect to non-HTTP(S) URL {target}"
    );
    Ok(Some(target))
}

fn remove_headers(headers: &mut HeaderMap, names: &[&str]) {
    for name in names {
        headers.remove(*name);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::{HttpRequestExt as _, Uri};

    fn policy(hosts: &[&str]) -> EgressPolicyHandle {
        let (policy, errors) = EgressPolicy::from_host_strings(hosts);
        assert_eq!(errors, Vec::new());
        EgressPolicyHandle::new(policy)
    }

    fn host(raw: &str) -> RequestHost {
        RequestHost::parse(raw).expect("host should parse")
    }

    #[test]
    fn endpoint_is_loopback_only_for_this_machine() {
        // Whatever this calls local, the AI lane must let through with no
        // user policy at all; whatever it calls remote, the AI lane must stop.
        let empty_policy = policy(&[]);
        let allowed_on_the_ai_lane = |api_url: &str| {
            Url::parse(api_url)
                .ok()
                .is_some_and(|url| empty_policy.check_url(&url).is_ok())
        };
        for local in [
            "http://localhost:11434",
            " http://127.0.0.1:11434 ",
            "http://[::1]:11434",
            "http://127.8.9.10",
            "http://LOCALHOST:11434/api",
            "http://localhost.:11434",
            "http://127.1:11434",
            "http://2130706433:11434",
            "http://0x7f.1:11434",
            "http://0177.0.0.1:11434",
            "http://[::ffff:127.0.0.1]:11434",
            "http://[0:0:0:0:0:0:0:1]/v1",
            "https://localhost/ollama?x=1#y",
            "http://%6c%6fcalhost:11434",
        ] {
            assert!(endpoint_is_loopback(local), "{local}");
            assert!(allowed_on_the_ai_lane(local), "{local}");
        }
        for remote in [
            "http://192.168.1.10:11434",
            "https://ollama.example.com",
            "http://localhost.example.com:11434",
            "http://localhost.evil.example",
            "http://api.localhost:11434",
            "http://127.0.0.1.nip.io:11434",
            "http://127.0.0.1@evil.example/",
            "http://localhost:11434@evil.example/",
            "http://evil.example#@localhost",
            r"http://evil.example\@localhost",
            "http://0.0.0.0:11434",
            "http://[::]:11434",
            "http://[::ffff:10.0.0.1]:11434",
            "http://128.0.0.1",
            "http://2147483648",
            "localhost:11434",
            "127.0.0.1:11434",
            "//localhost:11434",
            "file://localhost/etc/passwd",
            "",
            "   ",
            "not a url",
            "http://",
        ] {
            assert!(!endpoint_is_loopback(remote), "{remote}");
            assert!(!allowed_on_the_ai_lane(remote), "{remote}");
        }
    }

    /// `Response<AsyncBody>` is not `Debug`, so `expect_err` is unavailable.
    #[track_caller]
    fn expect_error(result: anyhow::Result<Response<AsyncBody>>, message: &str) -> anyhow::Error {
        match result {
            Ok(response) => panic!("{message} (got status {})", response.status()),
            Err(error) => error,
        }
    }

    #[test]
    fn parses_exact_subdomain_and_ip_patterns() {
        assert_eq!(
            HostPattern::parse("api.anthropic.com").unwrap(),
            HostPattern::Exact("api.anthropic.com".into())
        );
        assert_eq!(
            HostPattern::parse("API.Anthropic.COM.").unwrap(),
            HostPattern::Exact("api.anthropic.com".into())
        );
        assert_eq!(
            HostPattern::parse("*.corp.example").unwrap(),
            HostPattern::Subdomain("corp.example".into())
        );
        assert_eq!(
            HostPattern::parse("münchen.de").unwrap(),
            HostPattern::Exact("xn--mnchen-3ya.de".into())
        );
        assert_eq!(
            HostPattern::parse("10.0.0.5").unwrap(),
            HostPattern::Ip("10.0.0.5".parse().unwrap())
        );
        assert_eq!(
            HostPattern::parse("[fd00::1]").unwrap(),
            HostPattern::Ip("fd00::1".parse().unwrap())
        );
        assert_eq!(
            HostPattern::parse("fd00::1").unwrap(),
            HostPattern::Ip("fd00::1".parse().unwrap())
        );
    }

    #[test]
    fn rejects_malformed_patterns() {
        for pattern in ["", "   ", "*.", "foo.*.com", "*.*", "a..b", "*.10.0.0.5"] {
            assert!(
                HostPattern::parse(pattern).is_err(),
                "{pattern:?} should be rejected"
            );
        }
    }

    #[test]
    fn empty_policy_allows_only_loopback() {
        let policy = policy(&[]);
        for allowed in [
            "127.0.0.1",
            "127.0.0.53",
            "::1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "localhost",
            "LOCALHOST",
            "localhost.",
        ] {
            assert!(policy.allows(&host(allowed)), "{allowed} should be allowed");
        }
        for denied in [
            "api.anthropic.com",
            "zed.dev",
            "192.168.1.10",
            "0.0.0.0",
            "evil.localhost",
            "notlocalhost",
        ] {
            assert!(!policy.allows(&host(denied)), "{denied} should be denied");
        }
    }

    #[test]
    fn matches_exact_and_subdomain_patterns() {
        let policy = policy(&["api.anthropic.com", "*.corp.example", "10.0.0.5"]);

        assert!(policy.allows(&host("api.anthropic.com")));
        assert!(policy.allows(&host("API.Anthropic.com.")));
        assert!(!policy.allows(&host("anthropic.com")));
        assert!(!policy.allows(&host("api.anthropic.com.evil.net")));

        assert!(policy.allows(&host("git.corp.example")));
        assert!(policy.allows(&host("a.b.corp.example")));
        // A subdomain wildcard never matches its own bare parent, and never a
        // host that merely ends with the same characters.
        assert!(!policy.allows(&host("corp.example")));
        assert!(!policy.allows(&host("notcorp.example")));

        assert!(policy.allows(&host("10.0.0.5")));
        assert!(!policy.allows(&host("10.0.0.6")));
    }

    #[track_caller]
    fn check(policy: &EgressPolicyHandle, url: &str) -> Result<(), EgressDenied> {
        policy.check_url(&Url::parse(url).expect("valid url"))
    }

    #[test]
    fn every_built_in_entry_is_well_formed() {
        assert_eq!(
            BUILTIN_PATTERNS.len(),
            BUILTIN_ALLOWLIST.len(),
            "a built-in host pattern failed to parse"
        );
        for entry in BUILTIN_ALLOWLIST {
            assert!(!entry.paths.is_empty(), "{} has no paths", entry.host);
            for path in entry.paths {
                assert!(
                    path.starts_with('/') && !path.ends_with('/'),
                    "{path:?} should be an absolute prefix without a trailing slash"
                );
            }
        }
    }

    #[test]
    fn the_extension_marketplace_works_with_an_empty_user_policy() {
        let policy = policy(&[]);
        for url in [
            "https://api.zed.dev/extensions?max_schema_version=1",
            "https://api.zed.dev/extensions/updates?ids=html",
            "https://api.zed.dev/extensions/html",
            "https://api.zed.dev/extensions/html/download?max_schema_version=1",
            "https://api.zed.dev/extensions/html/0.3.1/download",
            // Where the download endpoint redirects.
            "https://zed-extensions.nyc3.digitaloceanspaces.com/extensions/html/0.3.1/archive.tar.gz?x-id=GetObject",
        ] {
            assert!(check(&policy, url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn the_built_in_default_does_not_open_anything_else() {
        let policy = policy(&[]);
        for url in [
            // Same host, other services: telemetry, and whatever else lands
            // there later.
            "https://api.zed.dev/telemetry/events",
            "https://api.zed.dev/",
            // A path that merely starts with the same characters.
            "https://api.zed.dev/extensions-private/secret",
            // Neighbouring hosts, including the CDN alias of the same bucket,
            // which the API was never observed to redirect to.
            "https://zed.dev/extensions",
            "https://api-staging.zed.dev/extensions",
            "https://cloud.zed.dev/extensions",
            "https://zed-extensions.nyc3.cdn.digitaloceanspaces.com/extensions/html/0.3.1/archive.tar.gz",
            "https://evil.digitaloceanspaces.com/extensions/x",
            // Unrelated hosts stay denied.
            "https://telemetry.example/report",
        ] {
            assert!(check(&policy, url).is_err(), "{url} should be denied");
        }
    }

    #[test]
    fn a_path_prefix_cannot_be_walked_out_of() {
        let policy = policy(&[]);
        assert!(
            check(
                &policy,
                "https://api.zed.dev/extensions/../telemetry/events"
            )
            .is_err(),
            "`..` is resolved before the prefix is compared"
        );
    }

    #[test]
    fn the_acp_registry_index_and_icons_work_with_an_empty_user_policy() {
        let policy = policy(&[]);
        for url in [
            "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json",
            "https://cdn.agentclientprotocol.com/registry/v1/latest/claude-acp.svg",
            "https://cdn.agentclientprotocol.com/registry/v1/latest/antigravity-acp.svg",
        ] {
            assert!(check(&policy, url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn the_acp_registry_built_in_default_does_not_open_agent_downloads() {
        let policy = policy(&[]);
        for url in [
            // Every agent's actual binary/archive lives on a distribution host
            // that this table never lists — that is what the F2-5 consent flow
            // is for, not the built-in default.
            "https://github.com/owner/repo/releases/download/v1/agent.tar.gz",
            "https://dl.google.com/agy/agy_acp_server.exe",
            // A path that merely starts with the same characters.
            "https://cdn.agentclientprotocol.com/registry/v1-private/secret",
            "https://cdn.agentclientprotocol.com/",
            "https://cdn.agentclientprotocol.com/other/path",
            // Neighbouring hosts stay denied.
            "https://agentclientprotocol.com/registry/v1/latest/registry.json",
            "https://evil.cdn.agentclientprotocol.com/registry/v1/latest/registry.json",
        ] {
            assert!(check(&policy, url).is_err(), "{url} should be denied");
        }
    }

    #[test]
    fn the_update_release_path_matches_the_update_repo() {
        assert_eq!(
            UPDATE_RELEASES_PATH,
            format!("/repos/{UPDATE_REPO}/releases")
        );
    }

    #[test]
    fn the_update_check_works_with_an_empty_user_policy() {
        let policy = policy(&[]);
        for url in [
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/releases",
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/releases/latest",
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/releases/tags/v1.2.3",
        ] {
            assert!(check(&policy, url).is_ok(), "{url} should be allowed");
        }
    }

    #[test]
    fn the_update_check_built_in_default_does_not_open_the_download() {
        let policy = policy(&[]);
        for url in [
            // The binaries themselves: what the consent flow is for.
            "https://github.com/SeiyaFunaokaJP/ForgeKairos/releases/download/v1.2.3/forgekairos-windows-x86_64-setup.exe",
            "https://objects.githubusercontent.com/github-production-release-asset/1/2",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
            // Any other repository, including the upstream this fork does not
            // follow.
            "https://api.github.com/repos/zed-industries/zed/releases",
            // The repository this fork was published from before it was renamed.
            "https://api.github.com/repos/SeiyaFunaokaJP/zed-jp-enhance/releases",
            // Anything else on the API host.
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos",
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/contents/README.md",
            "https://api.github.com/user",
            // A path that merely starts with the same characters.
            "https://api.github.com/repos/SeiyaFunaokaJP/ForgeKairos/releases-private",
            // Neighbouring hosts stay denied.
            "https://raw.githubusercontent.com/SeiyaFunaokaJP/ForgeKairos/main/README.md",
        ] {
            assert!(check(&policy, url).is_err(), "{url} should be denied");
        }
    }

    #[test]
    fn a_github_download_needs_every_hop_it_can_redirect_to() {
        assert_eq!(
            hosts_required_to_download(
                "https://github.com/owner/repo/releases/download/v1/agent.tar.gz"
            ),
            vec![
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string()
            ],
        );
        // Any other host is taken at face value.
        assert_eq!(
            hosts_required_to_download("https://dl.google.com/agy/agy_acp_server.exe"),
            vec!["dl.google.com".to_string()],
        );
        // Canonicalized the same way the allowlist matches.
        assert_eq!(
            hosts_required_to_download("https://Downloads.Cursor.COM./x.zip"),
            vec!["downloads.cursor.com".to_string()],
        );
        // Nothing to add for a URL with no host to check.
        assert!(hosts_required_to_download("not a url").is_empty());
        assert!(hosts_required_to_download("file:///tmp/agent.tar.gz").is_empty());
    }

    #[test]
    fn a_github_release_listing_asks_for_the_download_hops_too() {
        // The language server flow starts at the API host and ends up on the
        // asset CDN, so a consent prompt raised at the first denial has to
        // cover the whole chain (ADR-F22).
        assert_eq!(
            hosts_required_to_download("https://api.github.com/repos/clangd/clangd/releases"),
            vec![
                "api.github.com".to_string(),
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ],
        );
    }

    #[test]
    fn egress_denied_hosts_reads_through_the_context_a_caller_added() {
        let denied = anyhow::Error::new(EgressDenied {
            host: "api.github.com".to_string(),
            url: "https://api.github.com/repos/clangd/clangd/releases".to_string(),
        })
        .context("fetching the latest clangd release")
        .context("starting language server clangd");
        assert_eq!(
            egress_denied_hosts(&denied),
            Some(vec![
                "api.github.com".to_string(),
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ]),
        );

        assert_eq!(
            egress_denied_hosts(&anyhow::Error::new(EgressDenied {
                host: "REGISTRY.npmjs.ORG.".to_string(),
                url: "https://registry.npmjs.org/typescript".to_string(),
            })),
            Some(vec!["registry.npmjs.org".to_string()]),
        );

        // A failure with some other cause must not raise a consent prompt.
        assert_eq!(
            egress_denied_hosts(&anyhow::anyhow!("connection reset by peer")),
            None,
        );
    }

    #[test]
    fn a_marked_message_restores_the_denial_a_stringifying_boundary_dropped() {
        let denied = EgressDenied {
            host: "github.com".to_string(),
            url: "https://github.com/clangd/clangd/releases/download/18.1.3/clangd.zip".to_string(),
        };
        // What the extension WASM host hands the guest, and what the guest
        // hands back wrapped in its own text.
        let flattened = format!(
            "failed to install language server: {}\n{denied}",
            egress_denied_marker_line(&denied)
        );

        assert_eq!(egress_denied_from_message(&flattened), Some(denied));
        assert_eq!(
            egress_denied_from_message(&flattened)
                .and_then(|denied| egress_denied_hosts(&anyhow::Error::new(denied))),
            Some(vec![
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ]),
        );

        assert_eq!(
            egress_denied_from_message("download failed with status 500"),
            None,
        );
        assert_eq!(egress_denied_from_message(EGRESS_DENIED_MARKER), None);
        assert_eq!(
            egress_denied_from_message(&format!("{EGRESS_DENIED_MARKER} github.com")),
            None,
        );
    }

    #[test]
    fn the_built_in_default_and_the_user_list_are_unioned() {
        let user_listed = policy(&["api.anthropic.com"]);
        assert!(check(&user_listed, "https://api.anthropic.com/v1/messages").is_ok());
        assert!(check(&user_listed, "https://api.zed.dev/extensions").is_ok());
        assert!(check(&user_listed, "https://api.openai.com/v1/models").is_err());

        // Listing the host explicitly widens it to the whole host, as it does
        // for any other entry.
        let whole_host = policy(&["api.zed.dev"]);
        assert!(check(&whole_host, "https://api.zed.dev/telemetry/events").is_ok());
    }

    #[test]
    fn invalid_entries_are_reported_and_skipped() {
        let (policy, errors) = EgressPolicy::from_host_strings(["good.example", "*.*", ""]);
        assert_eq!(policy.patterns().len(), 1);
        assert_eq!(errors.len(), 2);
        assert!(policy.allows(&host("good.example")));
    }

    struct StubClient {
        seen: Arc<Mutex<Vec<(Method, String, Option<RedirectPolicy>, HeaderMap)>>>,
        respond: Box<dyn Fn(usize, &Url) -> Response<AsyncBody> + Send + Sync>,
    }

    impl StubClient {
        fn new(
            respond: impl Fn(usize, &Url) -> Response<AsyncBody> + Send + Sync + 'static,
        ) -> (
            Arc<Self>,
            Arc<Mutex<Vec<(Method, String, Option<RedirectPolicy>, HeaderMap)>>>,
        ) {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let client = Arc::new(Self {
                seen: seen.clone(),
                respond: Box::new(respond),
            });
            (client, seen)
        }
    }

    impl HttpClient for StubClient {
        fn user_agent(&self) -> Option<&HeaderValue> {
            None
        }

        fn proxy(&self) -> Option<&Url> {
            None
        }

        fn send(
            &self,
            req: Request<AsyncBody>,
        ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
            let url = Url::parse(&req.uri().to_string()).expect("stub requires absolute URLs");
            let index = {
                let mut seen = self.seen.lock().expect("stub lock poisoned");
                seen.push((
                    req.method().clone(),
                    url.to_string(),
                    req.extensions().get::<RedirectPolicy>().cloned(),
                    req.headers().clone(),
                ));
                seen.len() - 1
            };
            let response = (self.respond)(index, &url);
            Box::pin(async move { Ok(response) })
        }
    }

    fn redirect_to(location: &str) -> Response<AsyncBody> {
        Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, location)
            .body(AsyncBody::empty())
            .expect("valid response")
    }

    fn ok() -> Response<AsyncBody> {
        Response::builder()
            .status(StatusCode::OK)
            .body(AsyncBody::empty())
            .expect("valid response")
    }

    fn get(uri: &str, redirects: RedirectPolicy) -> Request<AsyncBody> {
        Request::builder()
            .uri(uri.parse::<Uri>().expect("valid uri"))
            .follow_redirects(redirects)
            .body(AsyncBody::empty())
            .expect("valid request")
    }

    #[test]
    fn denies_unlisted_host_without_reaching_the_inner_client() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let client = AllowlistedHttpClient::new(inner, policy(&["api.anthropic.com"]));

        let error = expect_error(
            futures::executor::block_on(client.send(get(
                "https://telemetry.example/report",
                RedirectPolicy::NoFollow,
            ))),
            "request should be denied",
        );

        let denied = error
            .downcast_ref::<EgressDenied>()
            .expect("should be an EgressDenied");
        assert_eq!(denied.host, "telemetry.example");
        assert!(seen.lock().unwrap().is_empty(), "no request should be sent");
    }

    #[test]
    fn egress_policy_accessor_reaches_the_shared_handle() {
        let (inner, _seen) = StubClient::new(|_, _| ok());
        let handle = policy(&["api.anthropic.com"]);
        let client = AllowlistedHttpClient::new(inner.clone(), handle.clone());

        assert!(
            !client
                .egress_policy()
                .unwrap()
                .allows(&host("api.openai.com"))
        );
        handle.set(EgressPolicy::from_host_strings(["api.openai.com"]).0);
        assert!(
            client
                .egress_policy()
                .unwrap()
                .allows(&host("api.openai.com")),
            "the accessor must reach the same handle backing this client's checks, not a copy"
        );

        // A plain client that doesn't wrap `AllowlistedHttpClient` has no
        // allowlist to report.
        assert!(inner.egress_policy().is_none());
    }

    #[test]
    fn allows_listed_host_and_loopback() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let client = AllowlistedHttpClient::new(inner, policy(&["api.anthropic.com"]));

        for uri in [
            "https://api.anthropic.com/v1/messages",
            "http://127.0.0.1:11434/api/chat",
            "http://localhost:1234/v1/models",
        ] {
            let response =
                futures::executor::block_on(client.send(get(uri, RedirectPolicy::NoFollow)))
                    .expect("request should be allowed");
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[test]
    fn strips_the_redirect_policy_before_delegating() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        futures::executor::block_on(
            client.send(get("https://files.example/a", RedirectPolicy::FollowAll)),
        )
        .expect("request should be allowed");

        assert_eq!(
            seen.lock().unwrap()[0].2,
            Some(RedirectPolicy::NoFollow),
            "the inner client must never be allowed to follow a hop itself"
        );
    }

    #[test]
    fn refuses_a_redirect_that_leaves_the_allowlist() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                redirect_to("https://exfil.example/collect")
            } else {
                ok()
            }
        });
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        let error = expect_error(
            futures::executor::block_on(client.send(get(
                "https://files.example/download",
                RedirectPolicy::FollowAll,
            ))),
            "the redirect hop should be denied",
        );

        let denied = error
            .downcast_ref::<EgressDenied>()
            .expect("should be an EgressDenied");
        assert_eq!(denied.host, "exfil.example");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "only the first hop should have been sent"
        );
    }

    #[test]
    fn follows_a_redirect_that_stays_inside_the_allowlist() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                redirect_to("https://cdn.files.example/blob")
            } else {
                ok()
            }
        });
        let client =
            AllowlistedHttpClient::new(inner, policy(&["files.example", "*.files.example"]));

        let response = futures::executor::block_on(client.send(get(
            "https://files.example/download",
            RedirectPolicy::FollowAll,
        )))
        .expect("both hops are allowed");

        assert_eq!(response.status(), StatusCode::OK);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].1, "https://cdn.files.example/blob");
    }

    /// The shape the real install takes: `api.zed.dev` answers 307 with a
    /// pre-signed URL on a different host, and the empty user policy has to
    /// carry both hops.
    #[test]
    fn an_extension_download_follows_its_redirect_with_an_empty_user_policy() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                Response::builder()
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header(
                        header::LOCATION,
                        "https://zed-extensions.nyc3.digitaloceanspaces.com/extensions/html/0.3.1/archive.tar.gz?x-id=GetObject",
                    )
                    .body(AsyncBody::empty())
                    .expect("valid response")
            } else {
                ok()
            }
        });
        let client = AllowlistedHttpClient::new(inner, policy(&[]));

        let response = futures::executor::block_on(client.send(get(
            "https://api.zed.dev/extensions/html/download?max_schema_version=1",
            RedirectPolicy::FollowAll,
        )))
        .expect("both hops are covered by the built-in default");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn returns_the_redirect_response_when_following_is_off() {
        let (inner, seen) = StubClient::new(|_, _| redirect_to("https://exfil.example/collect"));
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        let response = futures::executor::block_on(
            client.send(get("https://files.example/rpc", RedirectPolicy::NoFollow)),
        )
        .expect("a 3xx is a normal response when redirects are not followed");

        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn enforces_the_caller_s_redirect_limit() {
        let (inner, seen) = StubClient::new(|index, _| {
            redirect_to(&format!("https://files.example/hop{}", index + 1))
        });
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        let error = expect_error(
            futures::executor::block_on(client.send(get(
                "https://files.example/hop0",
                RedirectPolicy::FollowLimit(2),
            ))),
            "the limit should be enforced",
        );

        assert!(error.to_string().contains("too many redirects"), "{error}");
        assert_eq!(seen.lock().unwrap().len(), 3, "initial request plus 2 hops");
    }

    #[test]
    fn drops_credentials_when_a_redirect_changes_host() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                redirect_to("https://mirror.example/blob")
            } else {
                ok()
            }
        });
        let client =
            AllowlistedHttpClient::new(inner, policy(&["files.example", "mirror.example"]));

        let request = Request::builder()
            .uri("https://files.example/blob")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("x-api-key", "secret")
            .follow_redirects(RedirectPolicy::FollowAll)
            .body(AsyncBody::empty())
            .expect("valid request");
        futures::executor::block_on(client.send(request)).expect("both hops are allowed");

        let seen = seen.lock().unwrap();
        assert!(seen[0].3.contains_key(header::AUTHORIZATION));
        assert!(!seen[1].3.contains_key(header::AUTHORIZATION));
        assert!(!seen[1].3.contains_key("x-api-key"));
    }

    #[test]
    fn a_post_redirected_by_303_becomes_a_get() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                Response::builder()
                    .status(StatusCode::SEE_OTHER)
                    .header(header::LOCATION, "https://files.example/result")
                    .body(AsyncBody::empty())
                    .expect("valid response")
            } else {
                ok()
            }
        });
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        let request = Request::builder()
            .method(Method::POST)
            .uri("https://files.example/submit")
            .header(header::CONTENT_TYPE, "application/json")
            .follow_redirects(RedirectPolicy::FollowAll)
            .body(AsyncBody::from(r#"{"a":1}"#.to_string()))
            .expect("valid request");
        futures::executor::block_on(client.send(request)).expect("both hops are allowed");

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, Method::POST);
        assert_eq!(seen[1].0, Method::GET);
        assert!(!seen[1].3.contains_key(header::CONTENT_TYPE));
    }

    #[test]
    fn rejects_a_relative_request_uri() {
        let (inner, _) = StubClient::new(|_, _| ok());
        let client = AllowlistedHttpClient::new(inner, policy(&["files.example"]));

        let error = expect_error(
            futures::executor::block_on(
                client.send(get("/relative/path", RedirectPolicy::NoFollow)),
            ),
            "a relative URI has no host to check",
        );
        assert!(error.to_string().contains("not an absolute URL"), "{error}");
    }

    /// ADR-F38: a language server fetching its own binary must not need the
    /// user to have listed GitHub.
    #[test]
    fn an_ide_lane_request_reaches_a_host_the_allowlist_refuses() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let client = ide_lane(Arc::new(AllowlistedHttpClient::new(inner, policy(&[]))));

        let response = futures::executor::block_on(client.send(get(
            "https://github.com/clangd/clangd/releases/download/18/clangd.zip",
            RedirectPolicy::NoFollow,
        )))
        .expect("the IDE lane is not gated");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_unlabelled_request_to_the_same_host_is_denied() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let client = AllowlistedHttpClient::new(inner, policy(&[]));

        let error = expect_error(
            futures::executor::block_on(client.send(get(
                "https://github.com/clangd/clangd/releases/download/18/clangd.zip",
                RedirectPolicy::NoFollow,
            ))),
            "an unstamped request must stay on the gated lane",
        );

        assert_eq!(
            error
                .downcast_ref::<EgressDenied>()
                .expect("should be an EgressDenied")
                .host,
            "github.com"
        );
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn an_explicit_ai_lane_stamp_is_gated_like_no_stamp_at_all() {
        let (inner, _seen) = StubClient::new(|_, _| ok());
        let client = Arc::new(LaneHttpClient::new(
            Arc::new(AllowlistedHttpClient::new(inner, policy(&[]))),
            EgressLane::Ai,
        ));

        let error = expect_error(
            futures::executor::block_on(client.send(get(
                "https://api.openai.com/v1/chat",
                RedirectPolicy::NoFollow,
            ))),
            "the AI lane is the gated one",
        );
        assert!(error.downcast_ref::<EgressDenied>().is_some(), "{error}");
    }

    /// The lane decides whether the host is checked and nothing else: a hop
    /// that changes host still drops the caller's credentials, and the inner
    /// client is still told not to follow anything itself.
    #[test]
    fn an_ide_lane_redirect_keeps_every_other_invariant() {
        let (inner, seen) = StubClient::new(|index, _| {
            if index == 0 {
                redirect_to("https://release-assets.githubusercontent.com/blob")
            } else {
                ok()
            }
        });
        let client = ide_lane(Arc::new(AllowlistedHttpClient::new(inner, policy(&[]))));

        let request = Request::builder()
            .uri("https://github.com/owner/repo/releases/download/v1/blob")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("x-api-key", "secret")
            .follow_redirects(RedirectPolicy::FollowAll)
            .body(AsyncBody::empty())
            .expect("valid request");
        futures::executor::block_on(client.send(request))
            .expect("neither hop is gated on this lane");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the decorator still walks the hops itself");
        assert_eq!(seen[0].2, Some(RedirectPolicy::NoFollow));
        assert_eq!(seen[1].2, Some(RedirectPolicy::NoFollow));
        assert!(seen[0].3.contains_key(header::AUTHORIZATION));
        assert!(!seen[1].3.contains_key(header::AUTHORIZATION));
        assert!(!seen[1].3.contains_key("x-api-key"));
    }

    #[test]
    fn the_lane_survives_the_url_and_proxy_wrappers() {
        let (inner, seen) = StubClient::new(|_, _| ok());
        let gated = Arc::new(HttpClientWithUrl::new(
            Arc::new(AllowlistedHttpClient::new(inner, policy(&[]))),
            "https://zed.dev",
            None,
        ));
        let laned = ide_lane_with_url(gated.clone());

        futures::executor::block_on(laned.send(get(
            "https://registry.npmjs.org/typescript",
            RedirectPolicy::NoFollow,
        )))
        .expect("the stamp has to reach the decorator through both wrappers");
        assert_eq!(seen.lock().unwrap().len(), 1);

        // The URL surface keeps working, and keeps tracking the original.
        assert_eq!(laned.base_url(), "https://zed.dev");
        gated.set_base_url("http://localhost:3000");
        assert_eq!(laned.base_url(), "http://localhost:3000");

        // …while the client it wraps is still gated for everyone else.
        expect_error(
            futures::executor::block_on(gated.send(get(
                "https://registry.npmjs.org/typescript",
                RedirectPolicy::NoFollow,
            ))),
            "the underlying client must still gate unstamped requests",
        );
    }

    #[test]
    fn the_lane_wrapper_still_reports_the_allowlist_it_wraps() {
        let (inner, _seen) = StubClient::new(|_, _| ok());
        let handle = policy(&["api.anthropic.com"]);
        let client = ide_lane(Arc::new(AllowlistedHttpClient::new(inner, handle.clone())));

        // `remote_server` reaches its policy this way to apply the list the IDE
        // pushes it (ADR-F11), and it now holds a laned client.
        let reached = client
            .egress_policy()
            .expect("the accessor must pass through");
        assert!(reached.allows(&host("api.anthropic.com")));
        assert!(!reached.allows(&host("api.openai.com")));

        handle.set(EgressPolicy::from_host_strings(["api.openai.com"]).0);
        assert!(
            reached.allows(&host("api.openai.com")),
            "the accessor must reach the handle backing this client, not a copy"
        );
    }

    fn endpoint() -> Url {
        Url::parse("http://127.0.0.1:54321").expect("valid endpoint")
    }

    #[test]
    fn child_process_proxy_env_sets_both_casings_and_exempts_loopback() {
        let mut env = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        assert!(apply_proxy_env(&mut env, &endpoint()));

        for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            assert_eq!(
                env.get(name).map(String::as_str),
                Some("http://127.0.0.1:54321/"),
                "{name} should point at the proxy"
            );
        }
        for name in ["NO_PROXY", "no_proxy"] {
            assert_eq!(
                env.get(name).map(String::as_str),
                Some("localhost,127.0.0.1,::1")
            );
        }
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    #[test]
    fn child_process_proxy_env_leaves_a_user_configured_proxy_alone() {
        // Any one of the variables, in any casing, means the user configured a
        // proxy for this child; a partial overwrite would leave it with two.
        for name in ["HTTPS_PROXY", "http_proxy", "ALL_PROXY", "no_proxy"] {
            let mut env = HashMap::from([(name.to_string(), "http://corp:3128".to_string())]);
            assert!(!apply_proxy_env(&mut env, &endpoint()), "{name}");
            assert_eq!(env.len(), 1, "{name}: nothing else should have been added");
            assert_eq!(env.get(name).map(String::as_str), Some("http://corp:3128"));
        }
    }

    #[test]
    fn env_override_detection_matches_what_the_injection_skips() {
        for name in CHILD_PROCESS_PROXY_VARS {
            let mut env = HashMap::from([(name.to_string(), "http://corp:3128".to_string())]);
            assert!(env_overrides_child_process_proxy(&env), "{name}");
            assert!(!apply_proxy_env(&mut env, &endpoint()), "{name}");
        }

        let mut env = HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        assert!(!env_overrides_child_process_proxy(&env));
        assert!(apply_proxy_env(&mut env, &endpoint()));
    }

    #[test]
    fn child_process_proxy_env_is_a_no_op_while_no_proxy_is_running() {
        // This is the only test that touches the process-wide slot; it is left
        // as it was found so the others stay independent of ordering.
        assert_eq!(child_process_proxy(), None);
        let mut env = HashMap::new();
        assert!(!apply_child_process_proxy_env(&mut env));
        assert!(env.is_empty());

        set_child_process_proxy(Some(endpoint()));
        assert!(apply_child_process_proxy_env(&mut env));
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:54321/")
        );
        set_child_process_proxy(None);
        assert_eq!(child_process_proxy(), None);
    }
}
