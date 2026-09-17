//! Agent-side glue for the [`sandbox`] crate.
//!
//! Centralizes the "should agent-run terminal commands be sandboxed for this
//! process?" check so the system prompt, the terminal tool, and any other
//! caller see the same answer (and so the `target_os` gate lives in one
//! place instead of scattered across the agent crate).
//!
//! The current policy is: enabled iff the user has the `sandboxing` feature
//! flag turned on, the project is local (or, ADR-F18, a remote Linux host
//! reached over SSH, WSL or Docker), the platform has an integration, and the
//! user has not turned sandboxing off (the `allow_unsandboxed` sandbox
//! setting). Setting `allow_unsandboxed` turns sandboxing off for the
//! model-facing surface entirely: the plain (non-sandboxed) `terminal` tool is
//! exposed and the system prompt omits the sandbox section, since every command
//! would run without a wrap anyway.
//!
//! Short of that setting (or a project kind without a sandbox), a command
//! never runs outside the sandbox: the model cannot ask for it (ADR-F41), and
//! when the sandbox cannot be created the user can only retry or cancel
//! (ADR-F46). Commands that do run unwrapped are never auto-approved; see
//! `crate::decide_terminal_permission`.
//!
//! macOS (Seatbelt), Linux (Bubblewrap), and Windows (the ADR-F18 sandbox
//! helper inside WSL) have real sandbox integrations; on platforms without one
//! the per-command wrap is a no-op, so commands run with the agent's ambient
//! permissions even when the flag is on.
//!
//! Naming note: this module is about agent terminal sandboxing specifically.
//! Other agent operations (e.g. file edits) are gated separately.

use agent_settings::{AgentSettings, SandboxPermissions};
use feature_flags::{FeatureFlagAppExt as _, SandboxingFeatureFlag};
use gpui::App;
use http_proxy::HostPattern;
use project::Project;
use sandbox::{HostFilesystemLocation, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy};
use settings::{GrantedWritePath, Settings};
use std::path::PathBuf;

// These live in `acp_thread` so that the terminals external agents ask Zed to
// run (ADR-F49, built in `agent_servers`, which can't depend on this crate) get
// exactly the same sandbox scope and the same reason on their cards.
pub use acp_thread::{sandbox_git_dirs, sandbox_worktree_writable_paths};
pub(crate) use acp_thread::{sandboxing_available_for_project, unsandboxed_execution_reason};

/// What sandbox a thread applies to agent terminal commands, as one value the
/// UI renders and enforcement builds from. "No sandbox" is its own variant
/// rather than a maximally-permissive [`SandboxPolicy`] so that a wide-open but
/// real sandbox (e.g. `allow_fs_write_all` + `allow_all_hosts`) stays
/// distinguishable from running with no sandbox at all. The sandbox still
/// enforces invariants such as read-only Git metadata, while unsandboxed commands
/// run with ambient permissions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadSandbox {
    /// No OS sandbox is applied; commands run with ambient permissions.
    Unsandboxed,
    /// A sandbox is applied with this scope.
    Sandboxed(SandboxPolicy),
}

impl ThreadSandbox {
    /// Whether no OS sandbox is applied.
    pub fn is_unsandboxed(&self) -> bool {
        matches!(self, ThreadSandbox::Unsandboxed)
    }

    /// Attach the project's protected paths to a sandboxed layer. The settings
    /// and grants don't know the project's `.git` locations, so the caller
    /// computes them via [`sandbox_git_dirs`]. A no-op for `Unsandboxed`.
    pub fn with_protected_paths(self, protected_paths: Vec<PathBuf>) -> ThreadSandbox {
        match self {
            ThreadSandbox::Unsandboxed => ThreadSandbox::Unsandboxed,
            ThreadSandbox::Sandboxed(policy) => {
                ThreadSandbox::Sandboxed(policy_with_protected_paths(policy, protected_paths))
            }
        }
    }
}

/// Attach the project's protected paths (see [`sandbox_git_dirs`]) to a
/// sandbox policy, such as the one this thread's grants establish.
pub(crate) fn policy_with_protected_paths(
    policy: SandboxPolicy,
    protected_paths: Vec<PathBuf>,
) -> SandboxPolicy {
    // Capture each protected location (pinning its inode / canonical path). A
    // location that can't be captured is dropped — fail-closed.
    let protected_paths = protected_paths
        .into_iter()
        .filter_map(|path| HostFilesystemLocation::capture(path).ok())
        .collect();
    policy.with_protected_paths(protected_paths)
}

/// The sandbox the user's persistent settings establish for every thread, as a
/// [`ThreadSandbox`]. The persistent `allow_unsandboxed` setting removes the
/// sandbox entirely; otherwise the writable-path and host grants form its
/// scope. The per-thread overrides come from [`ThreadSandboxGrants::to_policy`].
pub fn settings_thread_sandbox(persistent: &SandboxPermissions) -> ThreadSandbox {
    if persistent.allow_unsandboxed {
        ThreadSandbox::Unsandboxed
    } else {
        ThreadSandbox::Sandboxed(settings_sandbox_policy(persistent))
    }
}

/// Translate the persistent "allow always" sandbox settings into the
/// cross-platform [`SandboxPolicy`] used for display. This is the "from your
/// settings" half of the sandbox status surface; the per-thread overrides come
/// from [`ThreadSandboxGrants::to_policy`].
pub fn settings_sandbox_policy(persistent: &SandboxPermissions) -> SandboxPolicy {
    let fs = if persistent.allow_fs_write_all {
        SandboxFsPolicy::Unrestricted {
            protected_paths: Vec::new(),
        }
    } else {
        SandboxFsPolicy::Restricted {
            // Route each grant through the verifying reopen (or a fresh capture
            // for legacy bare-string grants) so the enforced path is the one
            // vetted at approval time. A grant that fails to reopen is logged
            // and dropped (fail-closed) rather than silently swallowed, then the
            // survivors are reduced to a minimal cover on their canonical paths.
            writable_paths: sandbox::normalize_host_filesystem_locations(
                persistent
                    .write_paths
                    .iter()
                    .filter_map(acp_thread::granted_write_path_to_location_or_log),
            ),
            protected_paths: Vec::new(),
            readable_paths: Vec::new(),
        }
    };
    let network = if persistent.allow_all_hosts {
        SandboxNetPolicy::Unrestricted
    } else if persistent.network_hosts.is_empty() {
        SandboxNetPolicy::Blocked
    } else {
        SandboxNetPolicy::Restricted {
            allowed_domains: persistent.network_hosts.clone(),
        }
    };
    SandboxPolicy { fs, network }
}

/// Whether the sandboxed terminal can be exposed for this project.
///
/// The persistent `allow_unsandboxed` setting turns sandboxing off for the
/// model-facing surface: when it's set we expose the plain `terminal` tool and
/// omit the sandbox section from the system prompt, because every command would
/// run without a wrap regardless (and, per ADR-F46, ask for approval each
/// time).
pub(crate) fn sandboxing_enabled_for_project(project: &Project, cx: &App) -> bool {
    sandboxing_available_for_project(project, cx)
        && !AgentSettings::get_global(cx)
            .sandbox_permissions
            .allow_unsandboxed
}

/// Whether agent-run terminal commands should be wrapped in an OS-level
/// sandbox for this process. See module docs for the policy.
pub(crate) fn sandboxing_enabled(cx: &App) -> bool {
    cx.has_flag::<SandboxingFeatureFlag>()
}

/// Network escalation requested for (or granted to) a sandboxed command.
///
/// Network access in the sandbox is allowlisted by hostname: by default
/// commands have no outbound network, and an escalation lifts that for a
/// specific set of hosts (or, as a broad escape hatch, every host). The host
/// patterns are exact hostnames (`github.com`) or leading-`*.` subdomain
/// wildcards (`*.npmjs.org`); they're validated when constructed so the
/// variants here always hold well-formed patterns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum NetworkRequest {
    /// No network escalation — the conversation's default (blocked) applies.
    #[default]
    None,
    /// Allow connections only to these host patterns.
    Hosts(Vec<HostPattern>),
    /// Allow connections to any host ("arbitrary network access").
    AnyHost,
}

impl NetworkRequest {
    /// Whether this asks for any network access beyond the default (blocked).
    pub fn is_requested(&self) -> bool {
        !matches!(self, NetworkRequest::None)
    }

    /// The host patterns this request names, or an empty slice for the
    /// `None`/`AnyHost` variants.
    fn host_patterns(&self) -> &[HostPattern] {
        match self {
            NetworkRequest::Hosts(hosts) => hosts,
            NetworkRequest::None | NetworkRequest::AnyHost => &[],
        }
    }
}

/// A request for elevated sandbox permissions for a single terminal command.
///
/// Built from the model-controlled `terminal` tool input after the user has
/// authorized the baseline command. All paths here have already been resolved
/// to absolute, canonicalized paths by the caller — never raw, model-provided
/// strings, and never the model-controlled working directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SandboxRequest {
    /// Outbound network access requested for this command.
    pub network: NetworkRequest,

    /// Allow unrestricted filesystem writes (the broad escape hatch).
    pub allow_fs_write_all: bool,
    /// Concrete paths the command needs to write to, each paired with the
    /// canonical target resolved at approval time. Each grants its whole
    /// subtree. These are never globs — write access is always a concrete path subtree
    pub write_paths: Vec<GrantedWritePath>,
}

impl SandboxRequest {
    /// Whether this request asks for anything beyond the default sandbox
    /// scope, and therefore needs user approval.
    pub fn needs_escalation(&self) -> bool {
        self.network.is_requested() || self.allow_fs_write_all || !self.write_paths.is_empty()
    }
}

/// In-memory record of the sandbox permissions the user approved "for the
/// rest of the thread".
///
/// Lives on the `Thread` and is shared (via `Rc<RefCell<…>>`) with each tool
/// call's event stream so a later command requesting an already-granted
/// permission can skip the approval prompt. Persistent "allow always" grants
/// are stored separately in [`SandboxPermissions`].
///
/// Unrestricted writes (`allow_fs_write_all`) are never granted for a thread:
/// writing anywhere but the protected paths is nearly the same as running
/// outside the sandbox, so they're only ever allowed once per command or
/// through the settings switch (ADR-F46).
#[derive(Default)]
pub(crate) struct ThreadSandboxGrants {
    /// Whether arbitrary-host network access has been granted for the thread.
    network_any_host: bool,
    /// Host patterns granted network access for the thread. Each covers its
    /// whole subdomain space; redundant entries are pruned on insert.
    network_hosts: Vec<HostPattern>,
    /// Paths granted write access for the thread, each paired with the canonical
    /// target resolved at approval time. Each covers its whole subtree; redundant
    /// children are pruned on insert (by canonical target).
    write_paths: Vec<GrantedWritePath>,
    /// Whether the user already accepted the weaker integrity guarantees of a
    /// sandbox containing a Windows drive (DrvFs) path in this thread, so the
    /// warning is not repeated for every command (決定 1).
    ///
    /// Deliberately absent from [`Self::to_db`]: this is not a permission
    /// grant but an acknowledgement of a caveat, and a reopened thread asks
    /// again.
    windows_fs_warning_accepted: bool,
}

impl ThreadSandboxGrants {
    /// Whether the union of thread grants and persistent "allow always" grants
    /// covers everything `request` asks for, so the command can run without
    /// prompting again.
    ///
    /// Network coverage uses host-pattern subsumption (`*.foo.com` covers
    /// `api.foo.com`); write coverage is pure subtree containment. Both are
    /// fully deterministic and never widen scope, because grants are concrete
    /// patterns/paths rather than globs.
    pub fn covers_with_persistent(
        &self,
        request: &SandboxRequest,
        persistent: &SandboxPermissions,
    ) -> bool {
        if !self.network_covered(&request.network, persistent) {
            return false;
        }

        if request.allow_fs_write_all && !persistent.allow_fs_write_all {
            return false;
        }
        // The full-access write setting covers any concrete write request at
        // the authorization layer; protected paths are enforced by the sandbox.
        if persistent.allow_fs_write_all {
            return true;
        }
        request.write_paths.iter().all(|request| {
            util::paths::path_within_subtree(
                request.canonical_or_requested(),
                self.write_paths
                    .iter()
                    .chain(persistent.write_paths.iter())
                    .map(|granted| granted.canonical_or_requested()),
            )
        })
    }

    /// Whether the requested network escalation is already granted by the
    /// thread grants unioned with persistent "allow always" grants.
    fn network_covered(&self, request: &NetworkRequest, persistent: &SandboxPermissions) -> bool {
        let any_host_granted = self.network_any_host || persistent.allow_all_hosts;
        match request {
            NetworkRequest::None => true,
            NetworkRequest::AnyHost => any_host_granted,
            NetworkRequest::Hosts(requested) => {
                if any_host_granted {
                    return true;
                }
                let persistent_hosts = parse_persistent_hosts(&persistent.network_hosts);
                requested.iter().all(|requested| {
                    self.network_hosts
                        .iter()
                        .chain(persistent_hosts.iter())
                        .any(|granted| granted.covers(requested))
                })
            }
        }
    }

    /// Translate the per-thread overrides into the cross-platform
    /// [`SandboxPolicy`] used for display. This is the "overridden in this
    /// thread" half of the sandbox status surface; the persistent half comes
    /// from [`settings_thread_sandbox`]. Thread grants only ever widen the
    /// sandbox, never remove it (ADR-F46), so this is always a policy.
    pub fn to_policy(&self) -> SandboxPolicy {
        let fs = SandboxFsPolicy::Restricted {
            // Route each grant through the verifying reopen (or a fresh capture
            // for legacy bare-string grants) so the enforced path is the one
            // vetted at approval time. A grant that fails to reopen is logged
            // and dropped (fail-closed) rather than silently swallowed, then the
            // survivors are reduced to a minimal cover on their canonical paths.
            writable_paths: sandbox::normalize_host_filesystem_locations(
                self.write_paths
                    .iter()
                    .filter_map(acp_thread::granted_write_path_to_location_or_log),
            ),
            protected_paths: Vec::new(),
            readable_paths: Vec::new(),
        };
        let network = if self.network_any_host {
            SandboxNetPolicy::Unrestricted
        } else if self.network_hosts.is_empty() {
            SandboxNetPolicy::Blocked
        } else {
            SandboxNetPolicy::Restricted {
                allowed_domains: self
                    .network_hosts
                    .iter()
                    .map(|host| host.to_string())
                    .collect(),
            }
        };
        SandboxPolicy { fs, network }
    }

    /// Serialize these grants for persistence in the thread's database row.
    /// Host patterns are written in canonical string form so they round-trip
    /// through [`HostPattern::parse`] on load.
    pub fn to_db(&self) -> crate::db::DbSandboxGrants {
        crate::db::DbSandboxGrants {
            write_paths: self.write_paths.clone(),
            network_hosts: self
                .network_hosts
                .iter()
                .map(|host| host.to_string())
                .collect(),
            network_any_host: self.network_any_host,
        }
    }

    /// Rebuild thread grants from the persisted form. Host patterns that no
    /// longer parse (e.g. after a hand-edit) are dropped with a warning rather
    /// than failing the whole thread load.
    pub fn from_db(db: &crate::db::DbSandboxGrants) -> Self {
        let mut network_hosts = Vec::new();
        for raw in &db.network_hosts {
            match HostPattern::parse(raw) {
                Ok(pattern) => insert_host_pattern(&mut network_hosts, pattern),
                Err(error) => {
                    log::warn!("ignoring invalid persisted sandbox network host '{raw}': {error}")
                }
            }
        }
        // ADR-F49 決定 40 の残: a thread's one-off grants are folded to the
        // same spelling settings grants are, so a path approved for the
        // thread under an old spelling (`\\wsl$\…`, a differently-cased
        // distro name, or `/`-separated) still covers the same folder as one
        // recorded after decision 34. Folding can make two distinct rows
        // collide, so they are re-inserted through the subtree dedup rather
        // than cloned straight across.
        let mut write_paths = Vec::new();
        for granted in &db.write_paths {
            insert_granted_subtree(&mut write_paths, fold_persisted_write_path(granted.clone()));
        }
        Self {
            network_any_host: db.network_any_host,
            network_hosts,
            write_paths,
            windows_fs_warning_accepted: false,
        }
    }

    /// Whether the Windows-drive warning was already accepted in this thread
    /// (決定 1).
    pub fn windows_fs_warning_accepted(&self) -> bool {
        self.windows_fs_warning_accepted
    }

    /// Remember that the user accepted the Windows-drive warning, so the rest
    /// of the thread's commands don't repeat it. Only 「続行」 records this;
    /// 「中止」 leaves it unset so the next command asks again.
    pub fn accept_windows_fs_warning(&mut self) {
        self.windows_fs_warning_accepted = true;
    }

    /// Record everything in `request` as granted for the rest of the thread,
    /// pruning entries that become redundant.
    pub fn record(&mut self, request: &SandboxRequest) {
        match &request.network {
            NetworkRequest::None => {}
            NetworkRequest::AnyHost => self.network_any_host = true,
            NetworkRequest::Hosts(hosts) => {
                for host in hosts {
                    insert_host_pattern(&mut self.network_hosts, host.clone());
                }
            }
        }
        // `allow_fs_write_all` is deliberately not recorded (see the struct docs).
        for granted in &request.write_paths {
            insert_granted_subtree(&mut self.write_paths, granted.clone());
        }
    }

    /// Compute the effective sandbox permissions to enforce for a command: the
    /// union of persistent "allow always" grants, thread grants, and this
    /// specific command's request.
    ///
    /// This is what makes standing grants "stick": every sandboxed command
    /// applies the accumulated grants, so the model can write to a previously
    /// approved path (or reach a previously approved host) without
    /// re-requesting it. Passing the current `request` in also covers "allow
    /// once" grants, which are enforced for this command without being recorded
    /// for the thread.
    pub fn effective_with_persistent(
        &self,
        request: &SandboxRequest,
        persistent: &SandboxPermissions,
    ) -> SandboxRequest {
        let network = if self.network_any_host
            || persistent.allow_all_hosts
            || matches!(request.network, NetworkRequest::AnyHost)
        {
            NetworkRequest::AnyHost
        } else {
            let mut hosts = Vec::new();
            for host in self
                .network_hosts
                .iter()
                .cloned()
                .chain(parse_persistent_hosts(&persistent.network_hosts))
                .chain(request.network.host_patterns().iter().cloned())
            {
                insert_host_pattern(&mut hosts, host);
            }
            if hosts.is_empty() {
                NetworkRequest::None
            } else {
                NetworkRequest::Hosts(hosts)
            }
        };

        let mut write_paths = persistent.write_paths.clone();
        for granted in self.write_paths.iter().chain(request.write_paths.iter()) {
            insert_granted_subtree(&mut write_paths, granted.clone());
        }
        SandboxRequest {
            network,
            allow_fs_write_all: persistent.allow_fs_write_all || request.allow_fs_write_all,
            write_paths,
        }
    }
}

/// Parse persisted host strings into patterns, dropping (and logging) any
/// that fail to validate. Persisted strings are written in canonical form
/// (see `persist_sandbox_always_permission`), so this normally succeeds; the
/// filter is defensive against hand-edited settings.
fn parse_persistent_hosts(raw: &[String]) -> Vec<HostPattern> {
    raw.iter()
        .filter_map(|host| match HostPattern::parse(host) {
            Ok(pattern) => Some(pattern),
            Err(error) => {
                log::warn!(
                    "ignoring invalid network host pattern '{host}' in sandbox settings: {error}"
                );
                None
            }
        })
        .collect()
}

/// ADR-F49 決定 40: fold a persisted thread grant's requested/resolved paths
/// to the same WSL spelling [`agent_settings`]'s `compile_sandbox_permissions`
/// folds settings grants to, mirroring `agent_settings::fold_wsl_grant`
/// (private to that crate, so duplicated here rather than exposed just for
/// this one caller).
fn fold_persisted_write_path(granted: GrantedWritePath) -> GrantedWritePath {
    GrantedWritePath {
        requested: fold_wsl_grant_spelling(granted.requested),
        resolved: granted.resolved.map(fold_wsl_grant_spelling),
        on_windows_fs: granted.on_windows_fs,
    }
}

fn fold_wsl_grant_spelling(path: PathBuf) -> PathBuf {
    path.to_str()
        .and_then(util::path_mapping::canonical_wsl_grant_spelling)
        .map_or(path, PathBuf::from)
}

/// Insert `granted` into a write-grant set, keeping it minimal by comparing each
/// entry's canonical (resolved) target. Mirrors [`util::paths::insert_subtree`]:
/// skip the new grant if an existing entry's canonical is an ancestor-or-equal
/// of it, and prune existing entries whose canonical descends from the new one.
fn insert_granted_subtree(subtrees: &mut Vec<GrantedWritePath>, granted: GrantedWritePath) {
    if subtrees.iter().any(|existing| {
        granted
            .canonical_or_requested()
            .starts_with(existing.canonical_or_requested())
    }) {
        return;
    }
    subtrees.retain(|existing| {
        !existing
            .canonical_or_requested()
            .starts_with(granted.canonical_or_requested())
    });
    subtrees.push(granted);
}

/// Insert `pattern` into a host-pattern set, keeping it minimal: skip it if an
/// existing entry already subsumes it, and drop existing entries it subsumes.
/// The host-pattern analogue of [`util::paths::insert_subtree`].
pub(crate) fn insert_host_pattern(set: &mut Vec<HostPattern>, pattern: HostPattern) {
    if set.iter().any(|existing| existing.covers(&pattern)) {
        return;
    }
    set.retain(|existing| !pattern.covers(existing));
    set.push(pattern);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// `sandbox::APP_DIRECTORY_NAME` duplicates `paths::APP_NAME` /
    /// `APP_NAME_LOWERCASE` (see the doc comment on
    /// `crates/sandbox/src/protected_paths.rs`'s `APP_DIRECTORY_NAME`) because
    /// the `sandbox` crate is also the static musl remote-server helper and
    /// keeps its dependency surface minimal. A rename that only updates
    /// `paths::APP_NAME` would silently stop protecting this fork's own
    /// config/data/state directory, so this test catches the drift: if it
    /// fails, update `APP_DIRECTORY_NAME` in
    /// `crates/sandbox/src/protected_paths.rs` to match.
    #[test]
    fn sandbox_protected_app_directory_matches_paths_app_name() {
        let expected = if cfg!(target_os = "linux") {
            paths::APP_NAME_LOWERCASE
        } else {
            paths::APP_NAME
        };
        assert_eq!(
            sandbox::APP_DIRECTORY_NAME,
            expected,
            "crates/sandbox/src/protected_paths.rs's APP_DIRECTORY_NAME must be updated to \
             track paths::APP_NAME / paths::APP_NAME_LOWERCASE"
        );
    }

    fn hosts(list: &[&str]) -> NetworkRequest {
        NetworkRequest::Hosts(
            list.iter()
                .map(|h| HostPattern::parse(h).unwrap())
                .collect(),
        )
    }

    fn request(network: NetworkRequest, all: bool, paths: &[&str]) -> SandboxRequest {
        SandboxRequest {
            network,
            allow_fs_write_all: all,
            write_paths: granted(paths),
        }
    }

    /// Bare-string grants (no resolved canonical) for the lexical
    /// coverage/record/dedup tests, which never build a real policy.
    fn granted(paths: &[&str]) -> Vec<GrantedWritePath> {
        paths
            .iter()
            .map(|p| GrantedWritePath::from_requested(PathBuf::from(p)))
            .collect()
    }

    /// A grant carrying the resolved canonical of a real directory, for the
    /// tests that build an actual policy (which reopens/captures real fds).
    fn resolved_grant(dir: &Path) -> GrantedWritePath {
        GrantedWritePath::resolved(
            dir.to_path_buf(),
            sandbox::resolve_canonical(dir).expect("resolve canonical temp dir"),
        )
    }

    #[test]
    fn settings_thread_sandbox_reflects_allow_unsandboxed() {
        let unsandboxed = SandboxPermissions {
            allow_unsandboxed: true,
            ..Default::default()
        };
        assert!(settings_thread_sandbox(&unsandboxed).is_unsandboxed());
        assert!(matches!(
            settings_thread_sandbox(&SandboxPermissions::default()),
            ThreadSandbox::Sandboxed(_)
        ));
    }

    /// Threads saved before ADR-F41 can carry `unsandboxed: true` (the model
    /// asked to run outside the sandbox and the user allowed it for the
    /// thread). That request no longer exists, so resuming such a thread must
    /// bring the sandbox back rather than keep honoring the old grant.
    #[test]
    fn legacy_unsandboxed_thread_grant_is_not_restored() {
        let db: crate::db::DbSandboxGrants =
            serde_json::from_value(serde_json::json!({ "unsandboxed": true }))
                .expect("a legacy grant row should still deserialize");
        let restored = ThreadSandboxGrants::from_db(&db);
        assert_eq!(restored.to_db(), crate::db::DbSandboxGrants::default());
    }

    /// Threads saved before ADR-F46 can carry `sandbox_fallback: true` (the
    /// sandbox could not be created and the user chose to run the thread's
    /// commands without one). That choice no longer exists, so such a thread
    /// resumes sandboxed while its other grants still load.
    #[test]
    fn legacy_fallback_thread_grant_is_not_restored() {
        let db: crate::db::DbSandboxGrants = serde_json::from_value(serde_json::json!({
            "sandbox_fallback": true,
            "network_hosts": ["github.com"],
        }))
        .expect("a legacy grant row should still deserialize");
        let restored = ThreadSandboxGrants::from_db(&db);
        assert_eq!(
            restored.network_hosts,
            vec![HostPattern::parse("github.com").unwrap()]
        );
        let saved_again =
            serde_json::to_string(&restored.to_db()).expect("thread grants should serialize");
        assert!(
            !saved_again.contains("sandbox_fallback"),
            "the legacy field must not be written back: {saved_again}"
        );
    }

    /// Threads saved before ADR-F46 can carry `allow_fs_write_all: true` from
    /// "allow for this thread" on unrestricted writes. Writing anywhere is
    /// nearly the same as running outside the sandbox, so that grant is not
    /// restored; the thread's other grants still load.
    #[test]
    fn legacy_write_all_thread_grant_is_not_restored() {
        let db: crate::db::DbSandboxGrants = serde_json::from_value(serde_json::json!({
            "allow_fs_write_all": true,
            "write_paths": ["/tmp/build"],
        }))
        .expect("a legacy grant row should still deserialize");
        let restored = ThreadSandboxGrants::from_db(&db);
        assert!(!covers(
            &restored,
            &request(NetworkRequest::None, true, &[])
        ));
        assert!(covers(
            &restored,
            &request(NetworkRequest::None, false, &["/tmp/build/cache"])
        ));
        let saved_again =
            serde_json::to_string(&restored.to_db()).expect("thread grants should serialize");
        assert!(
            !saved_again.contains("allow_fs_write_all"),
            "the legacy field must not be written back: {saved_again}"
        );
    }

    #[test]
    fn grants_roundtrip_through_db_form() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(
            hosts(&["github.com", "*.npmjs.org"]),
            false,
            &["/tmp/build"],
        ));

        let restored = ThreadSandboxGrants::from_db(&grants.to_db());

        // The restored grants cover exactly what the originals did.
        assert!(covers(
            &restored,
            &request(hosts(&["api.npmjs.org"]), false, &["/tmp/build/cache"])
        ));
        assert_eq!(restored.network_hosts, grants.network_hosts);
        assert_eq!(restored.write_paths, grants.write_paths);
    }

    /// Unrestricted writes are never kept for the thread (ADR-F46), so an
    /// approval that also asked for them only keeps its network grant.
    #[test]
    fn db_form_preserves_any_host_but_never_write_all() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::AnyHost, true, &[]));

        let restored = ThreadSandboxGrants::from_db(&grants.to_db());
        assert!(restored.network_any_host);
        assert!(covers(
            &restored,
            &request(NetworkRequest::AnyHost, false, &[])
        ));
        assert!(!covers(
            &restored,
            &request(NetworkRequest::None, true, &[])
        ));
    }

    #[test]
    fn thread_grants_to_policy_maps_paths_and_domains() {
        use sandbox::{SandboxFsPolicy, SandboxNetPolicy};

        // `to_policy` reopens/captures real `HostFilesystemLocation`s, so use a
        // real dir and a grant carrying its resolved canonical.
        let build_dir = tempfile::tempdir().expect("create temp build dir");

        let mut grants = ThreadSandboxGrants::default();
        grants.record(&SandboxRequest {
            network: hosts(&["github.com"]),
            allow_fs_write_all: false,
            write_paths: vec![resolved_grant(build_dir.path())],
        });
        let policy = grants.to_policy();
        assert_eq!(
            policy.fs,
            SandboxFsPolicy::Restricted {
                writable_paths: vec![
                    HostFilesystemLocation::capture(build_dir.path()).expect("capture temp dir")
                ],
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            }
        );
        assert_eq!(
            policy.network,
            SandboxNetPolicy::Restricted {
                allowed_domains: vec!["github.com".to_string()]
            }
        );

        // No grants at all: writes restricted to nothing, network blocked.
        let empty = ThreadSandboxGrants::default().to_policy();
        assert_eq!(
            empty.fs,
            SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            }
        );
        assert_eq!(empty.network, SandboxNetPolicy::Blocked);

        // Any-host maps to the unrestricted network. Unrestricted writes are
        // never kept for the thread (ADR-F46), so writes stay restricted.
        let mut broad = ThreadSandboxGrants::default();
        broad.record(&request(NetworkRequest::AnyHost, true, &[]));
        let policy = broad.to_policy();
        assert_eq!(
            policy.fs,
            SandboxFsPolicy::Restricted {
                writable_paths: Vec::new(),
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            }
        );
        assert_eq!(policy.network, SandboxNetPolicy::Unrestricted);
    }

    #[test]
    fn settings_policy_maps_persistent_permissions() {
        use sandbox::{SandboxFsPolicy, SandboxNetPolicy};

        // `settings_sandbox_policy` captures real `HostFilesystemLocation`s.
        let log_dir = tempfile::tempdir().expect("create temp log dir");
        let persistent = SandboxPermissions {
            write_paths: vec![resolved_grant(log_dir.path())],
            network_hosts: vec!["*.npmjs.org".to_string()],
            ..Default::default()
        };
        let policy = settings_sandbox_policy(&persistent);
        assert_eq!(
            policy.fs,
            SandboxFsPolicy::Restricted {
                writable_paths: vec![
                    HostFilesystemLocation::capture(log_dir.path()).expect("capture temp dir")
                ],
                protected_paths: Vec::new(),
                readable_paths: Vec::new(),
            }
        );
        assert_eq!(
            policy.network,
            SandboxNetPolicy::Restricted {
                allowed_domains: vec!["*.npmjs.org".to_string()]
            }
        );

        let unrestricted = SandboxPermissions {
            allow_all_hosts: true,
            allow_fs_write_all: true,
            ..Default::default()
        };
        let policy = settings_sandbox_policy(&unrestricted);
        assert_eq!(
            policy.fs,
            SandboxFsPolicy::Unrestricted {
                protected_paths: Vec::new(),
            }
        );
        assert_eq!(policy.network, SandboxNetPolicy::Unrestricted);
    }

    #[test]
    fn db_form_drops_unparsable_persisted_hosts() {
        let db = crate::db::DbSandboxGrants {
            // IP literals are explicitly rejected by the host-pattern parser.
            network_hosts: vec!["github.com".to_string(), "10.0.0.1".to_string()],
            ..Default::default()
        };
        let restored = ThreadSandboxGrants::from_db(&db);
        assert_eq!(
            restored.network_hosts,
            vec![HostPattern::parse("github.com").unwrap()]
        );
    }

    /// ADR-F49 決定 40 の残: a thread's persisted one-off grants fold to the
    /// same spelling settings grants do, so a grant recorded before decision
    /// 34 (old `\\wsl$\` root, an upper-cased distro name, or a forward-slash
    /// UNC path) still covers the same folder as one recorded after. Two rows
    /// that name the same folder in two spellings become one.
    #[test]
    fn from_db_folds_old_wsl_grant_spellings() {
        let db = crate::db::DbSandboxGrants {
            write_paths: vec![
                GrantedWritePath::from_requested(PathBuf::from(r"\\wsl$\Ubuntu\home\one")),
                GrantedWritePath::from_requested(PathBuf::from(r"\\wsl.localhost\UBUNTU\home\two")),
                GrantedWritePath::from_requested(PathBuf::from(
                    "//wsl.localhost/Ubuntu/home/three",
                )),
                GrantedWritePath::from_requested(PathBuf::from(r"\\wsl$\UBUNTU\home\one")),
            ],
            ..Default::default()
        };
        let restored = ThreadSandboxGrants::from_db(&db);
        assert_eq!(
            restored.write_paths,
            vec![
                GrantedWritePath::from_requested(PathBuf::from(r"\\wsl.localhost\ubuntu\home\one")),
                GrantedWritePath::from_requested(PathBuf::from(r"\\wsl.localhost\ubuntu\home\two")),
                GrantedWritePath::from_requested(PathBuf::from(
                    r"\\wsl.localhost\ubuntu\home\three"
                )),
            ]
        );
    }

    /// ADR-F49 決定 40 の残: when folding makes one row the ancestor of
    /// another, the re-insertion applies the same rule the settings side does
    /// (`agent_settings::insert_granted_subtree`): the ancestor wins whichever
    /// order the rows are in, and the descendant's `on_windows_fs` goes with
    /// it — a warning, never a permission, and the same loss decision 40
    /// already records for the settings migration.
    ///
    /// Windows only: `Path::starts_with` can only see inside a `\\server\share`
    /// spelling under the host's own rules, so the two rows stay separate
    /// elsewhere (wider by one redundant row, never by a folder).
    #[cfg(target_os = "windows")]
    #[test]
    fn from_db_folds_grants_that_become_nested() {
        let ancestor = GrantedWritePath::from_requested(PathBuf::from(r"\\wsl$\Ubuntu\home"));
        let descendant = GrantedWritePath {
            requested: PathBuf::from(r"\\wsl.localhost\ubuntu\home\sub"),
            resolved: None,
            on_windows_fs: true,
        };
        let folded_ancestor =
            GrantedWritePath::from_requested(PathBuf::from(r"\\wsl.localhost\ubuntu\home"));

        for write_paths in [
            vec![ancestor.clone(), descendant.clone()],
            vec![descendant, ancestor],
        ] {
            let db = crate::db::DbSandboxGrants {
                write_paths,
                ..Default::default()
            };
            assert_eq!(
                ThreadSandboxGrants::from_db(&db).write_paths,
                vec![folded_ancestor.clone()]
            );
        }
    }

    fn covers(grants: &ThreadSandboxGrants, request: &SandboxRequest) -> bool {
        grants.covers_with_persistent(request, &SandboxPermissions::default())
    }

    fn effective(grants: &ThreadSandboxGrants, request: &SandboxRequest) -> SandboxRequest {
        grants.effective_with_persistent(request, &SandboxPermissions::default())
    }

    #[test]
    fn empty_grants_cover_nothing() {
        let grants = ThreadSandboxGrants::default();
        assert!(!covers(
            &grants,
            &request(NetworkRequest::AnyHost, false, &[])
        ));
        assert!(!covers(
            &grants,
            &request(hosts(&["github.com"]), false, &[])
        ));
        assert!(!covers(&grants, &request(NetworkRequest::None, true, &[])));
        assert!(!covers(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp/build"])
        ));
    }

    #[test]
    fn subtree_containment_covers_children() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build"]));

        // Exact match and any descendant are covered.
        assert!(covers(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp/build"])
        ));
        assert!(covers(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp/build/cache"])
        ));
        // A sibling / parent is not.
        assert!(!covers(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp/other"])
        ));
        assert!(!covers(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp"])
        ));
    }

    #[test]
    fn record_prunes_redundant_children() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build/cache"]));
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build"]));
        assert_eq!(grants.write_paths, granted(&["/tmp/build"]));
    }

    #[test]
    fn record_keeps_existing_broader_grant() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build"]));
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build/cache"]));
        assert_eq!(grants.write_paths, granted(&["/tmp/build"]));
    }

    #[test]
    fn thread_grants_never_record_unrestricted_writes() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, true, &[]));
        assert!(!covers(&grants, &request(NetworkRequest::None, true, &[])));
        assert!(!covers(
            &grants,
            &request(NetworkRequest::None, false, &["/anywhere/at/all"])
        ));
    }

    #[test]
    fn any_host_grant_covers_specific_and_any_host() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::AnyHost, false, &[]));
        assert!(covers(
            &grants,
            &request(NetworkRequest::AnyHost, false, &[])
        ));
        assert!(covers(
            &grants,
            &request(hosts(&["github.com"]), false, &[])
        ));
        // ...but not an orthogonal write request.
        assert!(!covers(
            &grants,
            &request(NetworkRequest::AnyHost, false, &["/tmp/build"])
        ));
    }

    #[test]
    fn host_grant_covers_subdomains_but_not_any_host() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(hosts(&["*.github.com"]), false, &[]));

        assert!(covers(
            &grants,
            &request(hosts(&["api.github.com"]), false, &[])
        ));
        assert!(covers(
            &grants,
            &request(hosts(&["*.github.com"]), false, &[])
        ));
        // The bare parent isn't a subdomain, so it isn't covered.
        assert!(!covers(
            &grants,
            &request(hosts(&["github.com"]), false, &[])
        ));
        // A different host isn't covered.
        assert!(!covers(
            &grants,
            &request(hosts(&["npmjs.org"]), false, &[])
        ));
        // A specific grant never satisfies an any-host request.
        assert!(!covers(
            &grants,
            &request(NetworkRequest::AnyHost, false, &[])
        ));
    }

    #[test]
    fn record_prunes_redundant_hosts() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(hosts(&["api.github.com"]), false, &[]));
        grants.record(&request(hosts(&["*.github.com"]), false, &[]));
        assert_eq!(
            grants.network_hosts,
            vec![HostPattern::parse("*.github.com").unwrap()]
        );
    }

    #[test]
    fn persistent_grants_combine_with_thread_grants() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(hosts(&["github.com"]), false, &[]));
        let persistent = SandboxPermissions {
            write_paths: granted(&["/tmp/build"]),
            ..Default::default()
        };

        assert!(grants.covers_with_persistent(
            &request(hosts(&["github.com"]), false, &["/tmp/build/cache"]),
            &persistent
        ));
        assert!(!grants.covers_with_persistent(
            &request(hosts(&["github.com"]), false, &["/tmp/other"]),
            &persistent
        ));
    }

    #[test]
    fn persistent_network_hosts_are_honored() {
        let grants = ThreadSandboxGrants::default();
        let persistent = SandboxPermissions {
            network_hosts: vec!["*.npmjs.org".to_string()],
            ..Default::default()
        };

        assert!(grants.covers_with_persistent(
            &request(hosts(&["registry.npmjs.org"]), false, &[]),
            &persistent
        ));
        assert!(
            !grants
                .covers_with_persistent(&request(hosts(&["github.com"]), false, &[]), &persistent)
        );
    }

    #[test]
    fn persistent_all_access_covers_concrete_writes() {
        let grants = ThreadSandboxGrants::default();
        let persistent = SandboxPermissions {
            allow_fs_write_all: true,
            ..Default::default()
        };

        assert!(grants.covers_with_persistent(
            &request(NetworkRequest::None, false, &["/anywhere"]),
            &persistent
        ));
        assert!(
            grants.covers_with_persistent(&request(NetworkRequest::None, true, &[]), &persistent)
        );
        assert!(
            !grants
                .covers_with_persistent(&request(NetworkRequest::AnyHost, false, &[]), &persistent)
        );
    }

    #[test]
    fn effective_applies_thread_grants_to_empty_request() {
        // The core fix: a command that requests nothing still gets the
        // thread's granted write paths in its enforced policy.
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build"]));

        let effective = effective(&grants, &request(NetworkRequest::None, false, &[]));
        assert_eq!(effective.write_paths, granted(&["/tmp/build"]));
    }

    #[test]
    fn effective_unions_grants_with_once_request() {
        // An "allow once" path (passed via `request`, never recorded) is
        // enforced for this command alongside the standing grants.
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(hosts(&["github.com"]), false, &["/tmp/build"]));

        let effective = effective(
            &grants,
            &request(hosts(&["npmjs.org"]), false, &["/tmp/once"]),
        );
        assert_eq!(effective.network, hosts(&["github.com", "npmjs.org"]));
        assert_eq!(effective.write_paths, granted(&["/tmp/build", "/tmp/once"]));
    }

    #[test]
    fn effective_any_host_subsumes_specific_hosts() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(hosts(&["github.com"]), false, &[]));

        let effective = effective(&grants, &request(NetworkRequest::AnyHost, false, &[]));
        assert_eq!(effective.network, NetworkRequest::AnyHost);
    }

    #[test]
    fn effective_applies_persistent_grants_to_empty_request() {
        let grants = ThreadSandboxGrants::default();
        let persistent = SandboxPermissions {
            allow_all_hosts: true,
            write_paths: granted(&["/tmp/always"]),
            ..Default::default()
        };

        let effective = grants
            .effective_with_persistent(&request(NetworkRequest::None, false, &[]), &persistent);
        assert_eq!(effective.network, NetworkRequest::AnyHost);
        assert_eq!(effective.write_paths, granted(&["/tmp/always"]));
    }

    #[test]
    fn effective_dedupes_request_already_covered_by_grant() {
        let mut grants = ThreadSandboxGrants::default();
        grants.record(&request(NetworkRequest::None, false, &["/tmp/build"]));

        let effective = effective(
            &grants,
            &request(NetworkRequest::None, false, &["/tmp/build/cache"]),
        );
        assert_eq!(effective.write_paths, granted(&["/tmp/build"]));
    }
}
