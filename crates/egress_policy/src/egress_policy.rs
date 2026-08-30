//! Reads and watches the egress allowlist policy file that drives
//! [`http_client::AllowlistedHttpClient`]. See `docs/jp-enhance/DESIGN.md` §4
//! (ADR-F03).
//!
//! The file lives next to `settings.json` (on Windows,
//! `%APPDATA%\Zed\network_allowlist.json`) and looks like:
//!
//! ```json
//! { "allowed_hosts": ["api.anthropic.com", "*.corp.example"] }
//! ```
//!
//! A missing, empty or unparsable file yields an empty policy — loopback only
//! — rather than an error, so a typo can never fail open and can never stop
//! the app from starting.
//!
//! What the file lists is unioned with [`http_client::BUILTIN_ALLOWLIST`], the
//! small path-scoped set the fork opens on its own (the extension
//! marketplace), so those hosts need no entry here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Global};
use http_client::{EgressPolicy, EgressPolicyHandle, HostPattern, RequestHost, Url};
use http_proxy::{DenyReason, DestinationPolicy, PolicyDecision};
use serde::Deserialize;

pub mod child_process_proxy;

/// The local IDE's live egress allowlist handle, published as a [`Global`] so
/// that code with no other path to it — `crates/remote`, pushing the
/// allowlist to a `remote_server` it just connected to — can read the current
/// policy without `egress_policy` having to depend on `remote` itself. See
/// `docs/jp-enhance/DESIGN.md` ADR-F11.
///
/// [`init`] and [`watch`] set this before returning the handle, so any code
/// that runs after the process's `HttpClient` has been built can rely on it
/// being present.
pub struct EgressAllowlistGlobal(pub EgressPolicyHandle);

impl Global for EgressAllowlistGlobal {}

/// The `proxy_child_processes` choice this process is running under.
///
/// `false` until [`init`] installs the local policy, and the policy file's
/// value from then on — which is `true` for an absent, empty or unusable file,
/// so the mechanism is opted out of rather than into. [`init`] sets it
/// synchronously, before the first read of the file has landed, so that a
/// remote connection made during startup already gets the right answer.
///
/// Starting at `false` is what makes a process that never installed a policy
/// of its own — a test harness standing up a `HeadlessProject` in-process —
/// not ask a remote host to run a proxy it has no counterpart for.
///
/// A process-wide slot rather than part of [`EgressAllowlistGlobal`] because
/// the writer is the watch task, which has no `App` to update a global with,
/// the same reason [`http_client::child_process_proxy`] is one.
static PROXY_CHILD_PROCESSES: AtomicBool = AtomicBool::new(false);

/// Whether the user wants agent-harness child processes pointed at the fork's
/// egress proxy. Read by `remote`, which forwards the choice to a
/// `remote_server` it has just connected to so that the ACP agents, MCP
/// servers and npm invocations running *there* are governed the same way
/// (ADR-F14).
///
/// Not the same question as `http_client::child_process_proxy().is_some()`,
/// which additionally reports whether the local listener came up — and which
/// is still `None` in the window between startup and the first read of the
/// policy file, where a remote connection can already be under way.
pub fn proxy_child_processes() -> bool {
    PROXY_CHILD_PROCESSES.load(Ordering::Relaxed)
}

/// On-disk shape of `network_allowlist.json`.
#[derive(Debug, Default, Deserialize)]
pub struct NetworkAllowlistFile {
    /// Host patterns: an exact hostname (`api.anthropic.com`), a leading-`*.`
    /// subdomain wildcard (`*.corp.example`, which does not match the bare
    /// parent), or an IP literal. Loopback is always permitted and does not
    /// need to be listed.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,

    /// Whether agent-harness child processes — ACP agents, MCP servers and
    /// npm — are pointed at the fork's loopback egress proxy so that
    /// `allowed_hosts` governs their traffic too (ADR-F13). Absent means on.
    ///
    /// It lives here rather than in `settings.json` because it is part of the
    /// egress policy, not an editor preference: this file is what the policy
    /// is written in, it is already watched live, and turning the mechanism
    /// off is the same kind of statement as removing a host from the list.
    /// Set it to `false` if a harness cannot cope with a proxy.
    #[serde(default)]
    pub proxy_child_processes: Option<bool>,
}

/// Everything `network_allowlist.json` says, as [`parse_file`] read it.
pub struct PolicyFile {
    pub policy: EgressPolicy,
    /// See [`NetworkAllowlistFile::proxy_child_processes`]. Defaults to `true`.
    pub proxy_child_processes: bool,
}

impl PolicyFile {
    /// What an absent, empty or unusable file means: loopback only, with the
    /// child-process proxy on.
    fn deny_all() -> Self {
        Self {
            policy: EgressPolicy::deny_all(),
            proxy_child_processes: true,
        }
    }
}

/// Starts reading and watching the user's policy file, returning the handle to
/// hand to [`http_client::AllowlistedHttpClient`].
///
/// The first read is asynchronous, so the policy is empty (loopback only) for
/// the moment between startup and the file landing. That direction is the safe
/// one: requests made in that window are denied, not waved through.
/// `upstream_proxy` is the user's own HTTP proxy (Zed's `proxy` setting, or
/// the environment), which the child-process proxy chains through.
pub fn init(fs: Arc<dyn Fs>, upstream_proxy: Option<&Url>, cx: &mut App) -> EgressPolicyHandle {
    // Installing the policy is what puts this process under the mechanism; the
    // file's own value replaces this as soon as the first read lands. See
    // [`PROXY_CHILD_PROCESSES`] for why the default lives here rather than in
    // the static.
    PROXY_CHILD_PROCESSES.store(true, Ordering::Relaxed);
    let handle = watch_inner(
        fs,
        paths::network_allowlist_file().clone(),
        Some(child_process_proxy::Supervisor::new(upstream_proxy)),
        cx,
    );
    // The terminal sandbox's per-command host list is nested inside this
    // allowlist rather than sitting beside it: a host approved for one
    // sandboxed command is reachable only if the app as a whole may reach it.
    // The handle is live, so editing the file re-aims the ceiling too.
    sandbox::set_network_ceiling(Some(Arc::new(SandboxNetworkCeiling {
        policy: handle.clone(),
    })));
    handle
}

/// The user's `allowed_hosts` (plus loopback) as the ceiling around the
/// terminal sandbox's own network allowlist — see
/// [`sandbox::set_network_ceiling`], which ANDs this with the sandbox's
/// decision.
///
/// Deliberately the *user's* policy only, exactly as with the child-process
/// proxy: [`http_client::BUILTIN_ALLOWLIST`] is path-scoped (`api.zed.dev`
/// only under `/extensions`, because telemetry shares that host) and a CONNECT
/// tunnel carries no path, so those entries cannot be evaluated here. They are
/// for the app's own fetches, which go through the decorator that does see
/// paths; a sandboxed command has no business in the extension marketplace.
struct SandboxNetworkCeiling {
    policy: EgressPolicyHandle,
}

impl DestinationPolicy for SandboxNetworkCeiling {
    fn evaluate(&self, host: &str, port: u16) -> PolicyDecision {
        let permitted = RequestHost::parse(host).is_some_and(|host| self.policy.allows(&host));
        if permitted {
            // Says only that *this* policy has no objection to a local
            // address; the sandbox's own decision is ANDed with it, and that
            // one keeps refusing loopback and private space.
            return PolicyDecision::Allow {
                permit_local_addresses: true,
            };
        }
        log::warn!(
            "egress allowlist: denied a sandboxed command's connection to {host}:{port} \
             — not on the network allowlist"
        );
        PolicyDecision::Deny(DenyReason::NotPermittedByPolicy {
            host: host.to_string(),
            explanation: format!("host '{host}' is not on the network allowlist"),
        })
    }
}

/// [`init`] against an explicit path.
///
/// Does not manage the child-process proxy (ADR-F13): its listener is a
/// whole-process resource belonging to the app's startup path, and a test that
/// watches a policy file has no business binding one.
pub fn watch(fs: Arc<dyn Fs>, path: PathBuf, cx: &mut App) -> EgressPolicyHandle {
    watch_inner(fs, path, None, cx)
}

fn watch_inner(
    fs: Arc<dyn Fs>,
    path: PathBuf,
    child_process_proxy: Option<child_process_proxy::Supervisor>,
    cx: &mut App,
) -> EgressPolicyHandle {
    let handle = EgressPolicyHandle::default();
    cx.set_global(EgressAllowlistGlobal(handle.clone()));
    let (mut contents, watcher) = settings::watch_config_file(cx.background_executor(), fs, path);
    cx.background_spawn({
        let handle = handle.clone();
        async move {
            // `watch_config_file` owns the file watch for as long as its task
            // is alive, so this task has to keep the task alive. The proxy
            // supervisor rides along for the same reason: the listener lives
            // exactly as long as the policy that governs it.
            let _watcher = watcher;
            let mut child_process_proxy = child_process_proxy;
            while let Some(contents) = contents.next().await {
                let file = parse_file(&contents);
                handle.set(file.policy);
                if let Some(supervisor) = child_process_proxy.as_mut() {
                    // Kept in step with the supervisor rather than with the
                    // file, so the flag always answers for a proxy that exists.
                    PROXY_CHILD_PROCESSES.store(file.proxy_child_processes, Ordering::Relaxed);
                    supervisor.apply(file.proxy_child_processes, &handle);
                }
            }
        }
    })
    .detach();
    handle
}

/// [`parse_file`], keeping only the host allowlist.
pub fn parse(contents: &str) -> EgressPolicy {
    parse_file(contents).policy
}

/// Parses the policy file's contents. Never fails: anything unusable is logged
/// and treated as "allow nothing but loopback".
pub fn parse_file(contents: &str) -> PolicyFile {
    if contents.trim().is_empty() {
        log::info!("egress allowlist: no policy file; allowing loopback only");
        return PolicyFile::deny_all();
    }

    let file: NetworkAllowlistFile = match settings_json::parse_json_with_comments(contents) {
        Ok(file) => file,
        Err(error) => {
            log::warn!(
                "egress allowlist: could not parse network_allowlist.json ({error:#}); \
                 allowing loopback only"
            );
            return PolicyFile::deny_all();
        }
    };

    let (policy, errors) = EgressPolicy::from_host_strings(&file.allowed_hosts);
    for error in errors {
        log::warn!("egress allowlist: skipping entry — {error}");
    }
    log::info!(
        "egress allowlist: {} host pattern(s) allowed in addition to loopback",
        policy.patterns().len()
    );
    PolicyFile {
        policy,
        proxy_child_processes: file.proxy_child_processes.unwrap_or(true),
    }
}

/// Adds `hosts` to `network_allowlist.json` at `path`, preserving whatever
/// else is in the file — including comments — and skipping hosts already
/// listed there. Each host is canonicalized the way [`HostPattern::parse`]
/// would; one that fails to parse is skipped rather than written verbatim,
/// the same direction every other typo in this file already fails in
/// (see [`parse`]): a bad entry can only fail to open a host, never corrupt
/// the list.
///
/// This is the shared mechanism behind two callers that cannot depend on each
/// other: the network allowlist settings page (`settings_ui`, ADR-F03) and
/// the F2-5 "install this agent" consent flow (`agent_ui`), which needs the
/// exact same comment-preserving read-modify-write without pulling in the
/// settings window machinery that page is built around.
///
/// Returns the file's `allowed_hosts` after the write. Fails on I/O, or when
/// the existing file cannot be parsed — deliberately refusing to touch it in
/// that case, so a rewrite never discards a hand-edit in progress.
pub async fn add_allowed_hosts(
    fs: &Arc<dyn Fs>,
    path: &Path,
    hosts: &[String],
) -> anyhow::Result<Vec<String>> {
    let exists = fs.is_file(path).await;
    let existing = if exists {
        fs.load(path)
            .await
            .context("reading network_allowlist.json")?
    } else {
        String::new()
    };

    let mut entries = if existing.trim().is_empty() {
        Vec::new()
    } else {
        settings_json::parse_json_with_comments::<NetworkAllowlistFile>(&existing)
            .context("parsing network_allowlist.json")?
            .allowed_hosts
    };

    for host in hosts {
        let Ok(pattern) = HostPattern::parse(host) else {
            log::warn!("egress allowlist: skipping invalid host {host:?}");
            continue;
        };
        let canonical = pattern.to_string();
        if !entries.iter().any(|entry| entry == &canonical) {
            entries.push(canonical);
        }
    }

    let indent_size = settings_json::infer_json_indent_size(&existing);
    let value = serde_json::Value::Array(
        entries
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect(),
    );
    let (range, replacement) = settings_json::replace_value_in_json_text(
        &existing,
        &["allowed_hosts"],
        indent_size,
        Some(&value),
        None,
    );
    anyhow::ensure!(
        range.end <= existing.len()
            && existing.is_char_boundary(range.start)
            && existing.is_char_boundary(range.end),
        "could not locate the allowed_hosts value to rewrite"
    );
    let mut new_text = existing;
    new_text.replace_range(range, &replacement);

    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    // `settings.json` is written the same way: to the canonical path, so that
    // the watch this crate registers on it (which canonicalizes too) sees it.
    let target = if exists {
        fs.canonicalize(path)
            .await
            .unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    fs.atomic_write(target, new_text)
        .await
        .context("writing network_allowlist.json")?;

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use gpui::TestAppContext;
    use http_client::{RequestHost, Url};

    #[track_caller]
    fn allows(policy: &EgressPolicy, host: &str) -> bool {
        policy.allows(&RequestHost::parse(host).expect("host should parse"))
    }

    #[test]
    fn parses_a_valid_policy() {
        let policy = parse(r#"{ "allowed_hosts": ["api.anthropic.com", "*.corp.example"] }"#);
        assert!(allows(&policy, "api.anthropic.com"));
        assert!(allows(&policy, "git.corp.example"));
        assert!(!allows(&policy, "corp.example"));
        assert!(!allows(&policy, "zed.dev"));
        // Loopback never needs listing.
        assert!(allows(&policy, "127.0.0.1"));
    }

    #[test]
    fn tolerates_comments_and_trailing_commas() {
        let policy = parse(
            r#"{
                // Cloud provider used for the agent.
                "allowed_hosts": ["api.anthropic.com",],
            }"#,
        );
        assert!(allows(&policy, "api.anthropic.com"));
    }

    #[test]
    fn a_missing_file_denies_everything_but_loopback() {
        for contents in ["", "   \n\t"] {
            let policy = parse(contents);
            assert!(policy.patterns().is_empty());
            assert!(allows(&policy, "localhost"));
            assert!(!allows(&policy, "api.anthropic.com"));
        }
    }

    #[test]
    fn garbage_denies_everything_but_loopback() {
        for contents in [
            "this is not json",
            "[]",
            r#"{ "allowed_hosts": "api.anthropic.com" }"#,
            r#"{ "allowed_hosts": [1, 2, 3] }"#,
        ] {
            let policy = parse(contents);
            assert!(
                policy.patterns().is_empty(),
                "{contents:?} should yield an empty policy"
            );
            assert!(!allows(&policy, "api.anthropic.com"));
            assert!(allows(&policy, "::1"));
        }
    }

    #[test]
    fn an_unknown_key_alone_is_not_fatal() {
        let policy = parse(r#"{ "allowed_hosts": ["api.anthropic.com"], "future_key": true }"#);
        assert!(allows(&policy, "api.anthropic.com"));
    }

    #[test]
    fn the_child_process_proxy_is_on_unless_the_file_turns_it_off() {
        // Absent, absent file, and unusable file all mean on: the mechanism
        // has to be opted *out* of, never opted into by accident.
        for contents in [
            "",
            "not json at all",
            r#"{ "allowed_hosts": [] }"#,
            r#"{ "allowed_hosts": ["api.anthropic.com"] }"#,
            r#"{ "proxy_child_processes": true }"#,
        ] {
            assert!(
                parse_file(contents).proxy_child_processes,
                "{contents:?} should leave the child-process proxy on"
            );
        }

        let file = parse_file(
            r#"{
                // A harness that cannot cope with a proxy.
                "allowed_hosts": ["api.anthropic.com"],
                "proxy_child_processes": false,
            }"#,
        );
        assert!(!file.proxy_child_processes);
        assert!(
            allows(&file.policy, "api.anthropic.com"),
            "turning the proxy off must not touch the host allowlist"
        );
    }

    #[test]
    fn the_forwarded_flag_stays_off_until_a_policy_is_installed() {
        // ADR-F14: what `remote` forwards to a remote host answers for a
        // supervisor this process actually has. `init` — the app's own
        // startup — is the only thing that turns it on, and nothing in this
        // test binary calls it, so a `HeadlessProject` stood up in-process is
        // never asked to bind a listener of its own.
        assert!(!proxy_child_processes());
    }

    #[test]
    fn invalid_entries_are_skipped_without_dropping_the_rest() {
        let policy = parse(r#"{ "allowed_hosts": ["good.example", "*.*", "", "10.0.0.5"] }"#);
        assert_eq!(policy.patterns().len(), 2);
        assert!(allows(&policy, "good.example"));
        assert!(allows(&policy, "10.0.0.5"));
    }

    #[track_caller]
    fn check(policy: &EgressPolicy, url: &str) -> bool {
        EgressPolicyHandle::new(policy.clone())
            .check_url(&Url::parse(url).expect("url should parse"))
            .is_ok()
    }

    #[test]
    fn the_extension_marketplace_needs_no_entry_in_the_file() {
        // Missing file, a file that lists something else, and a file that lists
        // nothing all keep the marketplace reachable, and none of them open the
        // rest of `api.zed.dev`.
        for contents in [
            "",
            r#"{ "allowed_hosts": [] }"#,
            r#"{ "allowed_hosts": ["api.anthropic.com"] }"#,
        ] {
            let policy = parse(contents);
            assert!(check(&policy, "https://api.zed.dev/extensions?filter=toml"));
            assert!(check(
                &policy,
                "https://zed-extensions.nyc3.digitaloceanspaces.com/extensions/toml/0.1.0/archive.tar.gz"
            ));
            assert!(!check(&policy, "https://api.zed.dev/telemetry/events"));
            assert!(!check(&policy, "https://zed.dev/extensions"));
        }

        // The built-in entries are not patterns in the user's list, so the
        // settings UI never shows them as removable entries.
        assert!(parse("").patterns().is_empty());
    }

    #[gpui::test]
    async fn reloads_when_the_policy_file_changes(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fs.insert_tree(
            std::path::Path::new(util::path!("/config")),
            serde_json::json!({
                "network_allowlist.json": r#"{ "allowed_hosts": ["api.anthropic.com"] }"#,
            }),
        )
        .await;

        let handle = cx.update(|cx| watch(fs.clone(), path.clone(), cx));
        cx.run_until_parked();

        let anthropic = RequestHost::parse("api.anthropic.com").expect("host should parse");
        let openai = RequestHost::parse("api.openai.com").expect("host should parse");
        assert!(handle.allows(&anthropic));
        assert!(!handle.allows(&openai));

        fs.insert_file(
            &path,
            br#"{ "allowed_hosts": ["api.openai.com"] }"#.to_vec(),
        )
        .await;
        cx.run_until_parked();

        assert!(!handle.allows(&anthropic));
        assert!(handle.allows(&openai));
    }

    #[gpui::test]
    async fn watch_publishes_the_same_handle_as_a_global(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let fs = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fs.insert_tree(
            std::path::Path::new(util::path!("/config")),
            serde_json::json!({}),
        )
        .await;

        let handle = cx.update(|cx| watch(fs.clone(), path.clone(), cx));
        cx.run_until_parked();

        let anthropic = RequestHost::parse("api.anthropic.com").expect("host should parse");
        cx.update(|cx| {
            let global = cx
                .try_global::<EgressAllowlistGlobal>()
                .expect("watch() should have published the global");
            assert!(
                !global.0.allows(&anthropic),
                "should start out empty like the returned handle"
            );
        });

        fs.insert_file(
            &path,
            br#"{ "allowed_hosts": ["api.anthropic.com"] }"#.to_vec(),
        )
        .await;
        cx.run_until_parked();

        cx.update(|cx| {
            let global = cx.try_global::<EgressAllowlistGlobal>().unwrap();
            assert!(
                global.0.allows(&anthropic),
                "the global must be the same handle `watch()` returned, not a snapshot of it"
            );
        });
        assert!(handle.allows(&anthropic));
    }

    #[gpui::test]
    async fn add_allowed_hosts_appends_without_duplicating(cx: &mut TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                std::path::Path::new(util::path!("/config")),
                serde_json::json!({
                    "network_allowlist.json": "// keep me\n{ \"allowed_hosts\": [\"*.corp.example\"] }\n"
                }),
            )
            .await;

        let entries = add_allowed_hosts(
            &fs,
            &path,
            &[
                "api.anthropic.com".to_string(),
                "*.corp.example".to_string(),
            ],
        )
        .await
        .expect("write should succeed");

        assert_eq!(
            entries,
            vec![
                "*.corp.example".to_string(),
                "api.anthropic.com".to_string()
            ],
            "an entry already present must not be duplicated, and the existing one must survive"
        );
        let on_disk = fake_fs.load(&path).await.expect("file should exist");
        assert!(
            on_disk.contains("// keep me"),
            "the comment outside allowed_hosts must survive the rewrite"
        );
    }

    #[gpui::test]
    async fn add_allowed_hosts_creates_a_missing_file(cx: &mut TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                std::path::Path::new(util::path!("/config")),
                serde_json::json!({}),
            )
            .await;

        let entries = add_allowed_hosts(&fs, &path, &["nodejs.org".to_string()])
            .await
            .expect("write should succeed");
        assert_eq!(entries, vec!["nodejs.org".to_string()]);

        let handle = cx.update(|cx| watch(fs.clone(), path.clone(), cx));
        cx.run_until_parked();
        assert!(handle.allows(&RequestHost::parse("nodejs.org").expect("host should parse")));
    }

    #[gpui::test]
    async fn add_allowed_hosts_skips_an_invalid_entry_instead_of_failing(cx: &mut TestAppContext) {
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                std::path::Path::new(util::path!("/config")),
                serde_json::json!({}),
            )
            .await;

        let entries =
            add_allowed_hosts(&fs, &path, &["good.example".to_string(), "*.*".to_string()])
                .await
                .expect("a bad entry must not fail the whole write");
        assert_eq!(entries, vec!["good.example".to_string()]);
    }

    #[gpui::test]
    async fn add_allowed_hosts_refuses_to_rewrite_an_unparsable_file(cx: &mut TestAppContext) {
        let broken = "{ \"allowed_hosts\": [\n";
        let fake_fs = fs::FakeFs::new(cx.background_executor.clone());
        let fs: Arc<dyn Fs> = fake_fs.clone();
        let path = PathBuf::from(util::path!("/config/network_allowlist.json"));
        fake_fs
            .insert_tree(
                std::path::Path::new(util::path!("/config")),
                serde_json::json!({ "network_allowlist.json": broken }),
            )
            .await;

        assert!(
            add_allowed_hosts(&fs, &path, &["zed.dev".to_string()])
                .await
                .is_err()
        );
        assert_eq!(
            fake_fs
                .load(&path)
                .await
                .expect("file should still be there"),
            broken,
            "a file that cannot be parsed must be left exactly as it was"
        );
    }

    /// The ceiling the terminal sandbox's own host list is nested inside is the
    /// user's allowlist plus loopback, read live: editing
    /// `network_allowlist.json` re-aims it with nothing re-published to the
    /// sandbox.
    #[test]
    fn the_sandbox_ceiling_follows_the_live_allowlist() {
        let handle = EgressPolicyHandle::default();
        let ceiling = SandboxNetworkCeiling {
            policy: handle.clone(),
        };

        assert!(matches!(
            ceiling.evaluate("api.anthropic.com", 443),
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy { .. })
        ));

        handle.set(parse(r#"{ "allowed_hosts": ["api.anthropic.com"] }"#));
        assert!(matches!(
            ceiling.evaluate("api.anthropic.com", 443),
            PolicyDecision::Allow { .. }
        ));
        assert!(matches!(
            ceiling.evaluate("api.openai.com", 443),
            PolicyDecision::Deny(DenyReason::NotPermittedByPolicy { .. })
        ));
        // This policy has no objection to loopback, as it has none for the
        // app's own requests. The sandbox's own decision, ANDed with this one,
        // is what still refuses it.
        assert!(matches!(
            ceiling.evaluate("127.0.0.1", 443),
            PolicyDecision::Allow { .. }
        ));
    }
}
