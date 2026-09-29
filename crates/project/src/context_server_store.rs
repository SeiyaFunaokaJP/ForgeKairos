pub mod extension;
pub mod registry;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use collections::{BTreeMap, HashMap, HashSet};
use context_server::oauth::{self, McpOAuthTokenProvider, OAuthDiscovery, OAuthSession};
use context_server::transport::HttpTransport;
use context_server::{ContextServer, ContextServerCommand, ContextServerId};
use credentials_provider::CredentialsProvider;
use futures::future::Either;
use futures::{FutureExt as _, StreamExt as _, future::join_all};
use gpui::{
    App, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task, TaskExt,
    WeakEntity, actions,
};
use http_client::HttpClient;
use itertools::Itertools;
use parking_lot::Mutex;
use rand::Rng as _;
use registry::ContextServerDescriptorRegistry;
use remote::{Interactive, RemoteClient};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use settings::{Settings as _, SettingsFile, SettingsLocation, SettingsStore, WorktreeId};
use util::{ResultExt as _, rel_path::RelPath};
use worktree::Worktree;

use crate::{
    DisableAiSettings, Project,
    project_launch_approvals::{
        CHANGED_SINCE_SHOWN_MESSAGE, CommandLineScan, ExecutionHost, LaunchApprovalDetails,
        LaunchApprovalKey, LaunchApprovalState, LaunchDecision, LaunchDefinitionFile, LaunchHost,
        LaunchKind, LaunchRequest, LaunchTarget, ProjectLaunchApprovals,
        ProjectLaunchApprovalsEvent, ScannedWorktrees, code_loading_environment,
        worktree_settings_file,
    },
    project_settings::{ContextServerSettings, OAuthClientSettings, ProjectSettings},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};

/// Maximum timeout for context server requests
/// Prevents extremely large timeout values from tying up resources indefinitely.
const MAX_TIMEOUT_SECS: u64 = 600; // 10 minutes

pub fn init(cx: &mut App) {
    extension::init(cx);
}

actions!(
    context_server,
    [
        /// Restarts the context server.
        Restart
    ]
);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContextServerStatus {
    Starting,
    Running,
    Stopped,
    Error(Arc<str>),
    /// The server returned 401 and OAuth authorization is needed. The UI
    /// should show an "Authenticate" button.
    AuthRequired,
    /// The server has a pre-registered OAuth client_id, but a client_secret
    /// is needed and not available in settings or the keychain.
    ClientSecretRequired {
        error: Option<Arc<str>>,
    },
    /// The OAuth browser flow is in progress — the user has been redirected
    /// to the authorization server and we're waiting for the callback.
    Authenticating,
    /// Defined by a project's settings, and this exact definition has not
    /// been approved (or was denied). Nothing is launched until it is; see
    /// [`ContextServerStore::launch_approval_details`] (ADR-F42).
    ApprovalRequired,
}

impl ContextServerStatus {
    fn from_state(state: &ContextServerState) -> Self {
        match state {
            ContextServerState::Starting { .. } => ContextServerStatus::Starting,
            ContextServerState::Running { .. } => ContextServerStatus::Running,
            ContextServerState::Stopped { .. } => ContextServerStatus::Stopped,
            ContextServerState::Error { error, .. } => ContextServerStatus::Error(error.clone()),
            ContextServerState::AuthRequired { .. } => ContextServerStatus::AuthRequired,
            ContextServerState::ClientSecretRequired { error, .. } => {
                ContextServerStatus::ClientSecretRequired {
                    error: error.clone(),
                }
            }
            ContextServerState::Authenticating { .. } => ContextServerStatus::Authenticating,
        }
    }
}

enum ContextServerState {
    Starting {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        _task: Task<()>,
    },
    Running {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        /// Initiates the OAuth flow if the transport shuts down on an
        /// authentication challenge; cancelled by any state transition.
        _transport_watch: Task<()>,
    },
    Stopped {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
    },
    Error {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        error: Arc<str>,
    },
    /// The server requires OAuth authorization before it can be used. The
    /// `OAuthDiscovery` holds everything needed to start the browser flow.
    AuthRequired {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        discovery: Arc<OAuthDiscovery>,
    },
    /// A pre-registered client_id is configured but no client_secret was found
    /// in settings or the keychain.
    ClientSecretRequired {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        discovery: Arc<OAuthDiscovery>,
        error: Option<Arc<str>>,
    },
    /// The OAuth browser flow is in progress. The user has been redirected
    /// to the authorization server and we're waiting for the callback.
    Authenticating {
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        _task: Task<()>,
    },
}

impl ContextServerState {
    pub fn server(&self) -> Arc<ContextServer> {
        match self {
            ContextServerState::Starting { server, .. }
            | ContextServerState::Running { server, .. }
            | ContextServerState::Stopped { server, .. }
            | ContextServerState::Error { server, .. }
            | ContextServerState::AuthRequired { server, .. }
            | ContextServerState::ClientSecretRequired { server, .. }
            | ContextServerState::Authenticating { server, .. } => server.clone(),
        }
    }

    pub fn configuration(&self) -> Arc<ContextServerConfiguration> {
        match self {
            ContextServerState::Starting { configuration, .. }
            | ContextServerState::Running { configuration, .. }
            | ContextServerState::Stopped { configuration, .. }
            | ContextServerState::Error { configuration, .. }
            | ContextServerState::AuthRequired { configuration, .. }
            | ContextServerState::ClientSecretRequired { configuration, .. }
            | ContextServerState::Authenticating { configuration, .. } => configuration.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ContextServerConfiguration {
    Custom {
        command: ContextServerCommand,
        remote: bool,
    },
    Extension {
        command: ContextServerCommand,
        settings: serde_json::Value,
        remote: bool,
    },
    Http {
        url: url::Url,
        headers: HashMap<String, String>,
        timeout: Option<u64>,
        oauth: Option<OAuthClientSettings>,
    },
}

impl ContextServerConfiguration {
    pub fn command(&self) -> Option<&ContextServerCommand> {
        match self {
            ContextServerConfiguration::Custom { command, .. } => Some(command),
            ContextServerConfiguration::Extension { command, .. } => Some(command),
            ContextServerConfiguration::Http { .. } => None,
        }
    }

    pub fn has_static_auth_header(&self) -> bool {
        match self {
            ContextServerConfiguration::Http { headers, .. } => headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization")),
            _ => false,
        }
    }

    pub fn remote(&self) -> bool {
        match self {
            ContextServerConfiguration::Custom { remote, .. } => *remote,
            ContextServerConfiguration::Extension { remote, .. } => *remote,
            ContextServerConfiguration::Http { .. } => false,
        }
    }

    pub async fn from_settings(
        settings: ContextServerSettings,
        id: ContextServerId,
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        cx: &AsyncApp,
    ) -> Option<Self> {
        const EXTENSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

        match settings {
            ContextServerSettings::Stdio {
                enabled: _,
                command,
                remote,
            } => Some(ContextServerConfiguration::Custom { command, remote }),
            ContextServerSettings::Extension {
                enabled: _,
                settings,
                remote,
            } => {
                let descriptor =
                    cx.update(|cx| registry.read(cx).context_server_descriptor(&id.0))?;

                let command_future = descriptor.command(worktree_store, cx);
                let timeout_future = cx.background_executor().timer(EXTENSION_COMMAND_TIMEOUT);

                match futures::future::select(command_future, timeout_future).await {
                    Either::Left((Ok(command), _)) => Some(ContextServerConfiguration::Extension {
                        command,
                        settings,
                        remote,
                    }),
                    Either::Left((Err(e), _)) => {
                        log::error!(
                            "Failed to create context server configuration from settings: {e:#}"
                        );
                        None
                    }
                    Either::Right(_) => {
                        log::error!(
                            "Timed out resolving command for extension context server {id}"
                        );
                        None
                    }
                }
            }
            ContextServerSettings::Http {
                enabled: _,
                url,
                headers: auth,
                timeout,
                oauth,
            } => {
                let url = url::Url::parse(&url).log_err()?;
                Some(ContextServerConfiguration::Http {
                    url,
                    headers: auth,
                    timeout,
                    oauth,
                })
            }
        }
    }
}

pub type ContextServerFactory =
    Box<dyn Fn(ContextServerId, Arc<ContextServerConfiguration>) -> Arc<ContextServer>>;

enum ContextServerStoreState {
    Local {
        downstream_client: Option<(u64, AnyProtoClient)>,
        is_headless: bool,
    },
    Remote {
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
    },
}

#[derive(Clone, PartialEq)]
struct ContextServerSettingsEntry {
    worktree_id: Option<WorktreeId>,
    settings: ContextServerSettings,
    source: ContextServerSource,
    replaces_user_entry: bool,
}

/// Where a server's definition comes from. Only `Project` definitions need
/// the user's approval before they are launched (ADR-F42).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextServerSource {
    /// The user's or the global settings.
    User,
    /// An installed extension's default.
    Extension,
    /// A worktree's `.zed/settings.json`, or the remote host's own settings.
    Project {
        worktree_id: WorktreeId,
        settings_file: LaunchDefinitionFile,
        host: LaunchHost,
    },
}

impl ContextServerSource {
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project { .. })
    }
}

pub struct ContextServerStore {
    state: ContextServerStoreState,
    context_server_settings: HashMap<Arc<str>, ContextServerSettingsEntry>,
    servers: HashMap<ContextServerId, ContextServerState>,
    server_ids: Vec<ContextServerId>,
    worktree_store: Entity<WorktreeStore>,
    project: Option<WeakEntity<Project>>,
    registry: Entity<ContextServerDescriptorRegistry>,
    update_servers_task: Option<Task<Result<()>>>,
    context_server_factory: Option<ContextServerFactory>,
    /// The working directory each server was last started with. The working
    /// directory of a stdio server depends on the resolved project root, which
    /// can only become available after the server has already started (e.g. a
    /// worktree is added moments after launch). Tracking it lets
    /// `maintain_servers` restart a server when its working directory changes,
    /// since the working directory is not part of `ContextServerConfiguration`.
    server_working_directories: HashMap<ContextServerId, Option<Arc<Path>>>,
    needs_server_update: bool,
    ai_disabled: bool,
    launch_host: LaunchHost,
    /// `None` on the remote server, which never starts servers itself.
    launch_approvals: Option<Entity<ProjectLaunchApprovals>>,
    /// The approval state of every enabled project-defined server, as found
    /// the last time the servers were brought in line with the settings.
    project_launches: HashMap<ContextServerId, LaunchApprovalDetails>,
    /// The approved configurations: the only ones `create_context_server`
    /// builds a project-defined server from.
    approved_project_configurations: HashMap<ContextServerId, Arc<ContextServerConfiguration>>,
    /// Approval state of the commands the remote host resolved on its own for
    /// `remote: true` extension servers, which nothing here can predict.
    remote_resolved_launches: HashMap<ContextServerId, LaunchApprovalDetails>,
    /// The configuration each server was last started from, as checked and
    /// before a `remote: true` command is wrapped for the remote host: what
    /// `maintain_servers` compares the settings with.
    started_configurations: HashMap<ContextServerId, Arc<ContextServerConfiguration>>,
    _subscriptions: Vec<Subscription>,
}

pub struct ServerStatusChangedEvent {
    pub server_id: ContextServerId,
    pub status: ContextServerStatus,
}

impl EventEmitter<ServerStatusChangedEvent> for ContextServerStore {}

impl ContextServerStore {
    pub fn local(
        worktree_store: Entity<WorktreeStore>,
        weak_project: Option<WeakEntity<Project>>,
        headless: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            !headless,
            None,
            ContextServerDescriptorRegistry::default_global(cx),
            worktree_store,
            weak_project,
            ContextServerStoreState::Local {
                downstream_client: None,
                is_headless: headless,
            },
            cx,
        )
    }

    pub fn remote(
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
        worktree_store: Entity<WorktreeStore>,
        weak_project: Option<WeakEntity<Project>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            true,
            None,
            ContextServerDescriptorRegistry::default_global(cx),
            worktree_store,
            weak_project,
            ContextServerStoreState::Remote {
                project_id,
                upstream_client,
            },
            cx,
        )
    }

    pub fn init_headless(session: &AnyProtoClient) {
        session.add_entity_request_handler(Self::handle_get_context_server_command);
    }

    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient) {
        if let ContextServerStoreState::Local {
            downstream_client, ..
        } = &mut self.state
        {
            *downstream_client = Some((project_id, client));
        }
    }

    pub fn is_remote_project(&self) -> bool {
        matches!(self.state, ContextServerStoreState::Remote { .. })
    }

    /// Returns all configured context server ids, excluding the ones that are disabled.
    /// Project-defined servers that wait for approval are included; they have
    /// no [`Self::configuration_for_server`], so nothing forwards them.
    pub fn configured_server_ids(&self) -> Vec<ContextServerId> {
        self.context_server_settings
            .iter()
            .filter(|(_, entry)| entry.settings.enabled())
            .map(|(id, _)| ContextServerId(id.clone()))
            .collect()
    }

    #[cfg(feature = "test-support")]
    pub fn test(
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        weak_project: Option<WeakEntity<Project>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            false,
            None,
            registry,
            worktree_store,
            weak_project,
            ContextServerStoreState::Local {
                downstream_client: None,
                is_headless: false,
            },
            cx,
        )
    }

    #[cfg(feature = "test-support")]
    pub fn test_maintain_server_loop(
        context_server_factory: Option<ContextServerFactory>,
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        weak_project: Option<WeakEntity<Project>>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_internal(
            true,
            context_server_factory,
            registry,
            worktree_store,
            weak_project,
            ContextServerStoreState::Local {
                downstream_client: None,
                is_headless: false,
            },
            cx,
        )
    }

    #[cfg(feature = "test-support")]
    pub fn set_context_server_factory(&mut self, factory: ContextServerFactory) {
        self.context_server_factory = Some(factory);
    }

    #[cfg(feature = "test-support")]
    pub fn registry(&self) -> &Entity<ContextServerDescriptorRegistry> {
        &self.registry
    }

    #[cfg(feature = "test-support")]
    pub fn working_directory_for_test(&self, id: &ContextServerId) -> Option<Option<Arc<Path>>> {
        self.server_working_directories.get(id).cloned()
    }

    #[cfg(feature = "test-support")]
    pub fn test_start_server(&mut self, server: Arc<ContextServer>, cx: &mut Context<Self>) {
        let configuration = Arc::new(ContextServerConfiguration::Custom {
            command: ContextServerCommand {
                path: "test".into(),
                args: vec![],
                env: None,
                timeout: None,
            },
            remote: false,
        });
        self.run_server(server, configuration, cx);
    }

    fn new_internal(
        maintain_server_loop: bool,
        context_server_factory: Option<ContextServerFactory>,
        registry: Entity<ContextServerDescriptorRegistry>,
        worktree_store: Entity<WorktreeStore>,
        weak_project: Option<WeakEntity<Project>>,
        state: ContextServerStoreState,
        cx: &mut Context<Self>,
    ) -> Self {
        let launch_host = match &state {
            ContextServerStoreState::Remote {
                upstream_client, ..
            } => LaunchHost::from_remote_connection(&upstream_client.read(cx).connection_options()),
            ContextServerStoreState::Local { .. } => LaunchHost::ThisMachine,
        };
        let is_headless = matches!(
            state,
            ContextServerStoreState::Local {
                is_headless: true,
                ..
            }
        );
        let launch_approvals = (!is_headless).then(|| ProjectLaunchApprovals::global(cx));

        let mut subscriptions = vec![cx.observe_global::<SettingsStore>(move |this, cx| {
            let ai_disabled = DisableAiSettings::get_global(cx).disable_ai;
            let ai_was_disabled = this.ai_disabled;
            this.ai_disabled = ai_disabled;

            let settings = Self::resolve_all_context_server_settings(
                &this.worktree_store,
                &this.launch_host,
                cx,
            );
            let settings_changed = this.context_server_settings != settings;

            if settings_changed {
                this.context_server_settings = settings;
            }

            // When AI is disabled, stop all running servers
            if ai_disabled {
                let server_ids: Vec<_> = this.servers.keys().cloned().collect();
                for id in server_ids {
                    this.stop_server(&id, cx).log_err();
                }
                return;
            }

            // Trigger updates if AI was re-enabled or settings changed
            if maintain_server_loop && (ai_was_disabled || settings_changed) {
                this.available_context_servers_changed(cx);
            }
        })];

        if maintain_server_loop {
            subscriptions.push(cx.observe(&registry, |this, _registry, cx| {
                if !DisableAiSettings::get_global(cx).disable_ai {
                    this.available_context_servers_changed(cx);
                }
            }));
            subscriptions.push(cx.subscribe(&worktree_store, |this, _store, event, cx| {
                if matches!(
                    event,
                    WorktreeStoreEvent::WorktreeAdded(_)
                        | WorktreeStoreEvent::WorktreeRemoved(_, _)
                ) && !DisableAiSettings::get_global(cx).disable_ai
                {
                    this.context_server_settings = Self::resolve_all_context_server_settings(
                        &this.worktree_store,
                        &this.launch_host,
                        cx,
                    );
                    this.available_context_servers_changed(cx);
                }
            }));
            if let Some(launch_approvals) = &launch_approvals {
                subscriptions.push(cx.subscribe(launch_approvals, |this, _, event, cx| {
                    let (key, lost_approval) = match event {
                        ProjectLaunchApprovalsEvent::LaunchDecided { key, decision } => {
                            (key, *decision == LaunchDecision::Denied)
                        }
                        ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(key) => (key, true),
                        ProjectLaunchApprovalsEvent::PendingLaunchAppeared(_)
                        | ProjectLaunchApprovalsEvent::PendingLaunchReplaced { .. }
                        | ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(_) => return,
                    };
                    // Decisions about what other stores and windows asked
                    // for leave this store's servers as they are.
                    let requested_here = this
                        .project_launches
                        .values()
                        .chain(this.remote_resolved_launches.values())
                        .any(|details| &details.key == key);
                    if !requested_here {
                        return;
                    }
                    // A remote-resolved command is only checked when it is
                    // started, so a running one that loses its approval is
                    // stopped here.
                    if lost_approval {
                        let withheld = this
                            .remote_resolved_launches
                            .iter()
                            .filter(|(_, details)| &details.key == key)
                            .map(|(id, _)| id.clone())
                            .collect::<Vec<_>>();
                        for id in withheld {
                            this.withhold_server(&id);
                            this.remote_resolved_launches.remove(&id);
                        }
                    }
                    if !DisableAiSettings::get_global(cx).disable_ai {
                        this.available_context_servers_changed(cx);
                    }
                }));
            }
        }
        if let Some(launch_approvals) = launch_approvals.clone() {
            let requester = cx.entity_id();
            cx.on_release(move |_, cx| {
                launch_approvals.update(cx, |launch_approvals, cx| {
                    launch_approvals.withdraw_requester(requester, cx)
                });
            })
            .detach();
        }

        let ai_disabled = DisableAiSettings::get_global(cx).disable_ai;
        let mut this = Self {
            state,
            _subscriptions: subscriptions,
            context_server_settings: Self::resolve_all_context_server_settings(
                &worktree_store,
                &launch_host,
                cx,
            ),
            worktree_store,
            project: weak_project,
            registry,
            needs_server_update: false,
            ai_disabled,
            servers: HashMap::default(),
            server_ids: Default::default(),
            update_servers_task: None,
            context_server_factory,
            server_working_directories: HashMap::default(),
            launch_host,
            launch_approvals,
            project_launches: HashMap::default(),
            approved_project_configurations: HashMap::default(),
            remote_resolved_launches: HashMap::default(),
            started_configurations: HashMap::default(),
        };
        if maintain_server_loop && !DisableAiSettings::get_global(cx).disable_ai {
            this.available_context_servers_changed(cx);
        }
        this
    }

    pub fn get_server(&self, id: &ContextServerId) -> Option<Arc<ContextServer>> {
        self.servers.get(id).map(|state| state.server())
    }

    pub fn get_running_server(&self, id: &ContextServerId) -> Option<Arc<ContextServer>> {
        if let Some(ContextServerState::Running { server, .. }) = self.servers.get(id) {
            Some(server.clone())
        } else {
            None
        }
    }

    pub fn status_for_server(&self, id: &ContextServerId) -> Option<ContextServerStatus> {
        if let Some(state) = self.servers.get(id) {
            return Some(ContextServerStatus::from_state(state));
        }
        self.launch_details(id)
            .filter(|details| details.state != LaunchApprovalState::Approved)
            .map(|_| ContextServerStatus::ApprovalRequired)
    }

    fn launch_details(&self, id: &ContextServerId) -> Option<&LaunchApprovalDetails> {
        self.project_launches
            .get(id)
            .or_else(|| self.remote_resolved_launches.get(id))
    }

    /// Where the server's definition comes from.
    pub fn server_source(&self, id: &ContextServerId, cx: &App) -> Option<ContextServerSource> {
        match self.context_server_settings.get(&id.0) {
            Some(entry) => Some(entry.source.clone()),
            None => self
                .registry
                .read(cx)
                .context_server_descriptor(&id.0)
                .map(|_| ContextServerSource::Extension),
        }
    }

    /// What an enabled server that needs approval would launch — one defined
    /// by a project, or a `remote: true` extension server whose command the
    /// remote host resolved — and whether that exact launch is approved.
    /// `None` for servers that need no approval.
    pub fn launch_approval_details(&self, id: &ContextServerId) -> Option<LaunchApprovalDetails> {
        self.launch_details(id).cloned()
    }

    /// Approves the launch shown for a server, keyed `shown`, which starts
    /// it. Refused with [`CHANGED_SINCE_SHOWN_MESSAGE`] when the server's
    /// launch has changed since. Resolves once the approval is stored.
    pub fn approve_server(
        &mut self,
        id: &ContextServerId,
        shown: &LaunchApprovalKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.decide_project_launch(id, shown, LaunchDecision::Approved, cx)
    }

    /// Denies the launch shown for a server, keyed `shown`; refused like
    /// [`Self::approve_server`]. The server stays stopped and the same launch
    /// does not notify again.
    pub fn deny_server(
        &mut self,
        id: &ContextServerId,
        shown: &LaunchApprovalKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.decide_project_launch(id, shown, LaunchDecision::Denied, cx)
    }

    /// Forgets the decision about the launch shown for a server, keyed
    /// `shown`; refused like [`Self::approve_server`]. A running server is
    /// stopped and taken out of the store, back to waiting for approval.
    pub fn revoke_server_approval(
        &mut self,
        id: &ContextServerId,
        shown: &LaunchApprovalKey,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (Some(details), Some(launch_approvals)) =
            (self.launch_details(id), self.launch_approvals.clone())
        else {
            return Task::ready(Err(no_project_launch_error(id)));
        };
        if &details.key != shown {
            return Task::ready(Err(anyhow!(CHANGED_SINCE_SHOWN_MESSAGE)));
        }
        let key = details.key.clone();
        launch_approvals.update(cx, |launch_approvals, cx| launch_approvals.revoke(&key, cx))
    }

    fn decide_project_launch(
        &mut self,
        id: &ContextServerId,
        shown: &LaunchApprovalKey,
        decision: LaunchDecision,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let (Some(details), Some(launch_approvals)) = (
            self.launch_details(id).cloned(),
            self.launch_approvals.clone(),
        ) else {
            return Task::ready(Err(no_project_launch_error(id)));
        };
        if &details.key != shown {
            return Task::ready(Err(anyhow!(CHANGED_SINCE_SHOWN_MESSAGE)));
        }
        launch_approvals.update(cx, |launch_approvals, cx| match decision {
            LaunchDecision::Approved => launch_approvals.approve(details, cx),
            LaunchDecision::Denied => launch_approvals.deny(details, cx),
        })
    }

    pub fn configuration_for_server(
        &self,
        id: &ContextServerId,
    ) -> Option<Arc<ContextServerConfiguration>> {
        self.servers.get(id).map(|state| state.configuration())
    }

    /// Returns the configured settings for a server, if it is present in the user
    /// or project settings. This is available regardless of whether the server is
    /// currently running, unlike [`Self::configuration_for_server`].
    pub fn settings_for_server(&self, id: &ContextServerId) -> Option<&ContextServerSettings> {
        self.context_server_settings
            .get(&id.0)
            .map(|entry| &entry.settings)
    }

    /// Returns whether a server is provided by an extension (as opposed to a
    /// custom Stdio/HTTP server configured directly in settings).
    ///
    /// This is derived from the configured settings rather than the runtime
    /// configuration, so it stays correct even when a custom server is disabled
    /// or has not been started yet (in which case it has no runtime state).
    pub fn is_extension_provided(&self, id: &ContextServerId, cx: &App) -> bool {
        match self.settings_for_server(id) {
            Some(ContextServerSettings::Stdio { .. } | ContextServerSettings::Http { .. }) => false,
            Some(ContextServerSettings::Extension { .. }) => true,
            // No custom settings entry: the server can only originate from an
            // extension descriptor in the registry.
            None => self
                .registry
                .read(cx)
                .context_server_descriptor(&id.0)
                .is_some(),
        }
    }

    /// Returns whether a server is enabled.
    /// Servers with no settings entry only originate from an extension
    /// descriptor in the registry, and those are enabled by default
    /// ([`ContextServerSettings::default_extension`]).
    pub fn is_server_enabled(&self, id: &ContextServerId, cx: &App) -> bool {
        match self.settings_for_server(id) {
            Some(settings) => settings.enabled(),
            None => self
                .registry
                .read(cx)
                .context_server_descriptor(&id.0)
                .is_some(),
        }
    }

    /// Returns a sorted slice of available unique context server IDs. Within the
    /// slice, context servers which have `mcp-server-` as a prefix in their ID will
    /// appear after servers that do not have this prefix in their ID.
    pub fn server_ids(&self) -> &[ContextServerId] {
        self.server_ids.as_slice()
    }

    fn populate_server_ids(&mut self, cx: &App) {
        self.server_ids = self
            .servers
            .keys()
            .cloned()
            .chain(
                self.registry
                    .read(cx)
                    .context_server_descriptors()
                    .into_iter()
                    .map(|(id, _)| ContextServerId(id)),
            )
            .chain(
                self.context_server_settings
                    .keys()
                    .map(|id| ContextServerId(id.clone())),
            )
            .unique()
            .sorted_unstable_by(
                // Sort context servers: ones without mcp-server- prefix first, then prefixed ones
                |a, b| {
                    const MCP_PREFIX: &str = "mcp-server-";
                    match (a.0.strip_prefix(MCP_PREFIX), b.0.strip_prefix(MCP_PREFIX)) {
                        // If one has mcp-server- prefix and other doesn't, non-mcp comes first
                        (Some(_), None) => std::cmp::Ordering::Greater,
                        (None, Some(_)) => std::cmp::Ordering::Less,
                        // If both have same prefix status, sort by appropriate key
                        (Some(a), Some(b)) => a.cmp(b),
                        (None, None) => a.0.cmp(&b.0),
                    }
                },
            )
            .collect();
    }

    pub fn running_servers(&self) -> Vec<Arc<ContextServer>> {
        self.servers
            .values()
            .filter_map(|state| {
                if let ContextServerState::Running { server, .. } = state {
                    Some(server.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn start_server(&mut self, server: Arc<ContextServer>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let this = this.upgrade().context("Context server store dropped")?;
            let id = server.id();
            let settings_entry = this
                .update(cx, |this, _| {
                    this.context_server_settings.get(&id.0).cloned()
                })
                .context("Failed to get context server settings")?;

            if !settings_entry.settings.enabled() {
                return anyhow::Ok(());
            }

            let (registry, worktree_store, root_path) = this.update(cx, |this, cx| {
                (
                    this.registry.clone(),
                    this.worktree_store.clone(),
                    this.resolve_root_path(cx),
                )
            });
            let configuration = ContextServerConfiguration::from_settings(
                settings_entry.settings.clone(),
                id.clone(),
                registry,
                worktree_store,
                cx,
            )
            .await
            .context("Failed to create context server configuration")?;
            let configuration = Arc::new(configuration);

            if !settings_entry.source.is_project() {
                this.update(cx, |this, cx| {
                    // Recorded as `maintain_servers` records its own starts,
                    // so that it leaves this server running.
                    let working_directory = working_directory_for(
                        &configuration,
                        root_path,
                        this.is_remote_project(),
                        false,
                    );
                    this.server_working_directories
                        .insert(id.clone(), working_directory);
                    this.started_configurations
                        .insert(id, configuration.clone());
                    this.run_server(server, configuration, cx)
                });
                return Ok(());
            }
            if !Self::project_launch_allowed(
                &this,
                &id,
                &configuration,
                &settings_entry,
                root_path.clone(),
                cx,
            )
            .await
            {
                return Ok(());
            }

            // The server handed in was built from whatever its caller read;
            // what runs is built from the definition just checked, in the
            // working directory it was checked with.
            let working_directory = this.update(cx, |this, _| {
                working_directory_for(
                    &configuration,
                    root_path.clone(),
                    this.is_remote_project(),
                    true,
                )
            });
            match Self::create_context_server_in(
                this.downgrade(),
                id.clone(),
                configuration.clone(),
                root_path,
                cx,
            )
            .await
            {
                Ok((server, started_configuration)) => {
                    this.update(cx, |this, cx| {
                        this.server_working_directories
                            .insert(id.clone(), working_directory);
                        this.started_configurations.insert(id, configuration);
                        this.run_server(server, started_configuration, cx)
                    });
                    Ok(())
                }
                Err(error) if error.is::<LaunchNeedsApproval>() => {
                    this.update(cx, |_, cx| {
                        cx.emit(ServerStatusChangedEvent {
                            server_id: id,
                            status: ContextServerStatus::ApprovalRequired,
                        });
                        cx.notify();
                    });
                    Ok(())
                }
                Err(error) => Err(error),
            }
        })
        .detach_and_log_err(cx);
    }

    /// The settings switch and the connection test start servers directly, so
    /// they get the same check `maintain_servers` makes. Returns whether the
    /// launch may go ahead.
    async fn project_launch_allowed(
        this: &Entity<Self>,
        id: &ContextServerId,
        configuration: &Arc<ContextServerConfiguration>,
        settings_entry: &ContextServerSettingsEntry,
        root_path: Option<Arc<Path>>,
        cx: &mut AsyncApp,
    ) -> bool {
        let (request, launch_approvals, fs) = this.update(cx, |this, cx| {
            (
                this.context_server_launch_request(
                    id,
                    configuration,
                    settings_entry,
                    root_path,
                    cx,
                ),
                this.launch_approvals.clone(),
                this.worktree_store.read(cx).fs(),
            )
        });
        let details = match request {
            Some(request) => Some(request.resolve(fs).await),
            None => None,
        };
        let requester = this.entity_id();
        let details = match (details, launch_approvals) {
            (Some(details), Some(launch_approvals)) => {
                Some(launch_approvals.update(cx, |launch_approvals, cx| {
                    launch_approvals.request_launch(details, requester, cx)
                }))
            }
            (details, _) => details,
        };
        let approved = details
            .as_ref()
            .is_some_and(|details| details.state == LaunchApprovalState::Approved);
        this.update(cx, |this, cx| {
            match details {
                Some(details) => {
                    this.project_launches.insert(id.clone(), details);
                }
                None => {
                    this.project_launches.remove(id);
                }
            }
            if approved {
                this.approved_project_configurations
                    .insert(id.clone(), configuration.clone());
            } else {
                this.withhold_server(id);
                cx.emit(ServerStatusChangedEvent {
                    server_id: id.clone(),
                    status: ContextServerStatus::ApprovalRequired,
                });
                cx.notify();
            }
        });
        approved
    }

    pub fn stop_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        if matches!(
            self.servers.get(id),
            Some(ContextServerState::Stopped { .. })
        ) {
            return Ok(());
        }

        let state = self
            .servers
            .remove(id)
            .context("Context server not found")?;

        let server = state.server();
        let configuration = state.configuration();
        let result = server.stop();
        drop(state);

        self.update_server_state(
            id.clone(),
            ContextServerState::Stopped {
                configuration,
                server,
            },
            cx,
        );

        result
    }

    fn run_server(
        &mut self,
        server: Arc<ContextServer>,
        configuration: Arc<ContextServerConfiguration>,
        cx: &mut Context<Self>,
    ) {
        let id = server.id();
        if matches!(
            self.servers.get(&id),
            Some(
                ContextServerState::Starting { .. }
                    | ContextServerState::Running { .. }
                    | ContextServerState::Authenticating { .. },
            )
        ) {
            self.stop_server(&id, cx).log_err();
        }
        let task = cx.spawn({
            let id = server.id();
            let server = server.clone();
            let configuration = configuration.clone();

            async move |this, cx| {
                let new_state = match server.clone().start(cx).await {
                    Ok(_) => {
                        debug_assert!(server.client().is_some());
                        let _transport_watch =
                            Self::watch_transport_shutdown(this.clone(), server.clone(), cx);
                        ContextServerState::Running {
                            server,
                            configuration,
                            _transport_watch,
                        }
                    }
                    Err(err) => resolve_start_failure(&id, err, server, configuration, cx).await,
                };
                this.update(cx, |this, cx| {
                    this.update_server_state(id.clone(), new_state, cx)
                })
                .log_err();
            }
        });

        self.update_server_state(
            id.clone(),
            ContextServerState::Starting {
                configuration,
                _task: task,
                server,
            },
            cx,
        );
    }

    /// Watches a running server's transport and initiates the OAuth flow if it
    /// shuts down on an authentication challenge.
    ///
    /// MCP servers may accept `initialize` unauthenticated and only send a 401
    /// with a `WWW-Authenticate` challenge on a later request or notification.
    /// The HTTP transport records the challenge, and the failed send tears
    /// down the client's output loop. Observing that shutdown — rather than
    /// relying on some request to carry a typed error back to a caller — is
    /// what lets any post-initialize 401 move the server into `AuthRequired`
    /// instead of leaving it `Running` with a dead client.
    fn watch_transport_shutdown(
        this: WeakEntity<Self>,
        server: Arc<ContextServer>,
        cx: &mut AsyncApp,
    ) -> Task<()> {
        let Some(shutdown) = server
            .client()
            .and_then(|client| client.wait_for_shutdown())
        else {
            return Task::ready(());
        };
        cx.spawn(async move |cx| {
            let Some(www_authenticate) = shutdown.await else {
                // Non-auth transport deaths leave the server state untouched,
                // as they did before this watch existed.
                return;
            };
            this.update(cx, |this, cx| {
                this.handle_auth_challenge(server, www_authenticate, cx);
            })
            .log_err();
        })
    }

    fn handle_auth_challenge(
        &mut self,
        server: Arc<ContextServer>,
        www_authenticate: oauth::WwwAuthenticate,
        cx: &mut Context<Self>,
    ) {
        let id = server.id();

        // Act only if this exact server is still the one we consider running.
        // If the state has changed since the challenge was recorded, whoever
        // changed it owns the lifecycle now.
        let Some(ContextServerState::Running {
            server: running_server,
            configuration,
            ..
        }) = self.servers.get(&id)
        else {
            return;
        };
        if !Arc::ptr_eq(running_server, &server) {
            return;
        }
        let configuration = configuration.clone();

        log::info!("{id} received 401 after initialization; initiating OAuth authorization");

        // The 401 already tore down the client's output loop. Stop the dead
        // client, then resolve auth using the captured `WWW-Authenticate` — do
        // not restart via `run_server`, as a fresh `initialize` would succeed
        // and lose the challenge.
        server.stop().log_err();

        let task = cx.spawn({
            let id = id.clone();
            let server = server.clone();
            let configuration = configuration.clone();
            async move |this, cx| {
                let new_state =
                    resolve_auth_required(&id, &www_authenticate, server, configuration, cx).await;
                this.update(cx, |this, cx| {
                    this.update_server_state(id.clone(), new_state, cx)
                })
                .log_err();
            }
        });

        self.update_server_state(
            id,
            ContextServerState::Starting {
                configuration,
                _task: task,
                server,
            },
            cx,
        );
    }

    fn remove_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        let state = self
            .servers
            .remove(id)
            .context("Context server not found")?;
        self.server_working_directories.remove(id);
        self.started_configurations.remove(id);

        if let ContextServerConfiguration::Http { url, .. } = state.configuration().as_ref() {
            let server_url = url.clone();
            let id = id.clone();
            cx.spawn(async move |_this, cx| {
                let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
                if let Err(err) = Self::clear_session(&credentials_provider, &server_url, &cx).await
                {
                    log::warn!("{} failed to clear OAuth session on removal: {}", id, err);
                }
            })
            .detach();
        }

        drop(state);
        cx.emit(ServerStatusChangedEvent {
            server_id: id.clone(),
            status: ContextServerStatus::Stopped,
        });
        cx.notify();
        Ok(())
    }

    /// Takes a server whose definition lost its approval out of the store,
    /// without the teardown `remove_server` does for a server deleted from
    /// settings: the OAuth session stays for when it is approved again.
    fn withhold_server(&mut self, id: &ContextServerId) {
        if let Some(state) = self.servers.remove(id) {
            state.server().stop().log_err();
        }
        self.server_working_directories.remove(id);
        self.started_configurations.remove(id);
    }

    /// Records the approval state found for the project-defined servers, and
    /// reports the ones that now wait for an approval or no longer do.
    fn update_project_launches(
        &mut self,
        project_launches: HashMap<ContextServerId, LaunchApprovalDetails>,
        withheld_servers: &HashSet<ContextServerId>,
        starting_servers: &HashSet<ContextServerId>,
        cx: &mut Context<Self>,
    ) {
        let previous_launches = std::mem::replace(&mut self.project_launches, project_launches);
        for (id, details) in &self.project_launches {
            let needs_approval = details.state != LaunchApprovalState::Approved;
            if needs_approval
                && (withheld_servers.contains(id) || previous_launches.get(id) != Some(details))
            {
                cx.emit(ServerStatusChangedEvent {
                    server_id: id.clone(),
                    status: ContextServerStatus::ApprovalRequired,
                });
            }
        }
        for (id, previous) in &previous_launches {
            let current = self.project_launches.get(id);
            if previous.state != LaunchApprovalState::Approved
                && current.is_none_or(|details| details.state == LaunchApprovalState::Approved)
                && !self.servers.contains_key(id)
                && !starting_servers.contains(id)
            {
                cx.emit(ServerStatusChangedEvent {
                    server_id: id.clone(),
                    status: ContextServerStatus::Stopped,
                });
            }
            if previous.state == LaunchApprovalState::Pending
                && current.is_none_or(|details| details.key != previous.key)
                && let Some(launch_approvals) = &self.launch_approvals
            {
                let requester = cx.entity_id();
                launch_approvals.update(cx, |launch_approvals, cx| {
                    launch_approvals.withdraw_pending(&previous.key, requester, cx)
                });
            }
        }
        cx.notify();
    }

    /// What a project-defined server would launch with `root_path` as the
    /// project root, for the approval key and the approval UI. `None` for
    /// servers that need no approval, and for a project-defined one whose
    /// worktree is gone (which then stays blocked).
    fn context_server_launch_request(
        &self,
        id: &ContextServerId,
        configuration: &ContextServerConfiguration,
        settings_entry: &ContextServerSettingsEntry,
        root_path: Option<Arc<Path>>,
        cx: &App,
    ) -> Option<LaunchRequest> {
        let ContextServerSource::Project {
            worktree_id,
            settings_file,
            host,
        } = &settings_entry.source
        else {
            return None;
        };
        let worktree_store = self.worktree_store.read(cx);
        let worktree_abs_path = worktree_store
            .worktree_for_id(*worktree_id, cx)?
            .read(cx)
            .abs_path();
        let worktrees = ScannedWorktrees::of(
            worktree_store
                .visible_worktrees(cx)
                .map(|worktree| worktree.read(cx)),
        );
        let is_remote_project = self.is_remote_project();

        let (
            definition,
            target,
            environment_variable_names,
            header_names,
            code_loading_environment,
            execution_host,
            scan,
        ) = match configuration {
            ContextServerConfiguration::Custom { command, remote }
            | ContextServerConfiguration::Extension {
                command, remote, ..
            } => {
                let runs_on_remote_host = *remote && is_remote_project;
                // The working directory it is spawned with: the project over
                // there for a server run there; for one spawned here the
                // project, or in a remote project the neutral directory, from
                // which no worktree can be reached.
                let (working_directory, worktrees) = if runs_on_remote_host {
                    (root_path, worktrees)
                } else if is_remote_project {
                    (
                        local_working_directory(true, true, root_path),
                        ScannedWorktrees::default(),
                    )
                } else {
                    (local_working_directory(false, true, root_path), worktrees)
                };
                let program = command.path.to_string_lossy().into_owned();
                let environment = command.env.iter().flatten().collect::<BTreeMap<_, _>>();
                let scan = CommandLineScan::new(
                    Some(program.as_str()),
                    &command.args,
                    environment
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str())),
                    working_directory.as_deref(),
                    worktrees,
                );
                let (transport, extension_settings) = match configuration {
                    ContextServerConfiguration::Extension { settings, .. } => {
                        ("extension", Some(settings))
                    }
                    _ => ("stdio", None),
                };
                (
                    serde_json::json!({
                        "transport": transport,
                        "program": program,
                        "arguments": command.args,
                        "env": environment,
                        "timeout": command.timeout,
                        "remote": remote,
                        "settings": extension_settings,
                    }),
                    LaunchTarget::Command {
                        program: program.into(),
                        arguments: command
                            .args
                            .iter()
                            .cloned()
                            .map(SharedString::from)
                            .collect(),
                    },
                    environment
                        .keys()
                        .map(|name| SharedString::from((*name).clone()))
                        .collect(),
                    Vec::new(),
                    code_loading_environment(
                        environment
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_str())),
                    ),
                    if runs_on_remote_host {
                        ExecutionHost::of(host)
                    } else {
                        ExecutionHost::ThisMachine
                    },
                    scan,
                )
            }
            ContextServerConfiguration::Http {
                url,
                headers,
                timeout,
                oauth,
            } => (
                serde_json::json!({
                    "transport": "http",
                    "url": url.as_str(),
                    "headers": headers.iter().collect::<BTreeMap<_, _>>(),
                    "timeout": timeout,
                    "oauth": oauth,
                }),
                LaunchTarget::Url(url.to_string().into()),
                Vec::new(),
                headers
                    .keys()
                    .map(|name| SharedString::from(name.clone()))
                    .collect(),
                Vec::new(),
                ExecutionHost::ThisMachine,
                CommandLineScan::new(None, &[], std::iter::empty(), None, worktrees),
            ),
        };

        Some(LaunchRequest {
            kind: LaunchKind::ContextServer,
            name: SharedString::from(id.0.clone()),
            definition,
            target,
            environment_variable_names,
            header_names,
            execution_host,
            host: host.clone(),
            definition_file: settings_file.clone(),
            worktree_abs_path,
            replaces_user_entry: settings_entry.replaces_user_entry,
            code_loading_environment,
            scan,
        })
    }

    /// The launch of a command the remote host resolved on its own for a
    /// `remote: true` extension server. Nothing here can predict it, so it is
    /// approved as it came back.
    fn remote_resolved_launch_request(
        &self,
        id: &ContextServerId,
        response: &proto::ContextServerCommand,
        extension_settings: &serde_json::Value,
        root_dir: Option<&Path>,
        cx: &App,
    ) -> Option<LaunchRequest> {
        let settings_entry = self.context_server_settings.get(&id.0);
        let worktree_store = self.worktree_store.read(cx);
        let worktree = settings_entry
            .and_then(|entry| entry.worktree_id)
            .and_then(|worktree_id| worktree_store.worktree_for_id(worktree_id, cx))
            .or_else(|| worktree_store.visible_worktrees(cx).next())?;
        let worktree_abs_path = worktree.read(cx).abs_path();
        let environment = response
            .env
            .iter()
            .filter(|(name, _)| !is_child_process_proxy_variable(name))
            .collect::<BTreeMap<_, _>>();
        let scan = CommandLineScan::new(
            Some(response.path.as_str()),
            &response.args,
            environment
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            root_dir,
            ScannedWorktrees::of(
                worktree_store
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx)),
            ),
        );
        Some(LaunchRequest {
            kind: LaunchKind::ContextServer,
            name: SharedString::from(id.0.clone()),
            definition: serde_json::json!({
                "transport": "extension",
                "resolved_on": "remote_host",
                "program": response.path,
                "arguments": response.args,
                "env": environment,
                "settings": extension_settings,
            }),
            target: LaunchTarget::Command {
                program: response.path.clone().into(),
                arguments: response
                    .args
                    .iter()
                    .cloned()
                    .map(SharedString::from)
                    .collect(),
            },
            environment_variable_names: environment
                .keys()
                .map(|name| SharedString::from((*name).clone()))
                .collect(),
            header_names: Vec::new(),
            code_loading_environment: code_loading_environment(
                environment
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            ),
            execution_host: ExecutionHost::of(&self.launch_host),
            host: self.launch_host.clone(),
            definition_file: LaunchDefinitionFile::ResolvedOnRemoteHost,
            worktree_abs_path,
            replaces_user_entry: false,
            scan,
        })
    }

    async fn approve_remote_resolved_command(
        this: WeakEntity<Self>,
        id: &ContextServerId,
        response: &proto::ContextServerCommand,
        extension_settings: &serde_json::Value,
        root_dir: Option<&Path>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (request, launch_approvals) = this.update(cx, |this, cx| {
            (
                this.remote_resolved_launch_request(id, response, extension_settings, root_dir, cx),
                this.launch_approvals.clone(),
            )
        })?;
        let (Some(request), Some(launch_approvals)) = (request, launch_approvals) else {
            return Err(anyhow::Error::new(LaunchNeedsApproval));
        };
        let details = request.resolve(None).await;
        let requester = this.entity_id();
        let details = launch_approvals.update(cx, |launch_approvals, cx| {
            launch_approvals.request_launch(details, requester, cx)
        });
        let approved = details.state == LaunchApprovalState::Approved;
        this.update(cx, |this, _| {
            this.remote_resolved_launches.insert(id.clone(), details);
        })?;
        if approved {
            Ok(())
        } else {
            Err(anyhow::Error::new(LaunchNeedsApproval))
        }
    }

    /// The project root a locally-spawned stdio server should use as its working
    /// directory: the active project directory, falling back to the first visible
    /// worktree. Resolves to `None` before any worktree is available.
    fn resolve_root_path(&self, cx: &App) -> Option<Arc<Path>> {
        self.project
            .as_ref()
            .and_then(|project| {
                project
                    .read_with(cx, |project, cx| project.active_project_directory(cx))
                    .ok()
                    .flatten()
            })
            .or_else(|| {
                self.worktree_store.read_with(cx, |store, cx| {
                    store.visible_worktrees(cx).fold(None, |acc, item| {
                        if acc.is_none() {
                            item.read(cx).root_dir()
                        } else {
                            acc
                        }
                    })
                })
            })
    }

    pub async fn create_context_server(
        this: WeakEntity<Self>,
        id: ContextServerId,
        configuration: Arc<ContextServerConfiguration>,
        cx: &mut AsyncApp,
    ) -> Result<(Arc<ContextServer>, Arc<ContextServerConfiguration>)> {
        let root_path = this.update(cx, |this, cx| this.resolve_root_path(cx))?;
        Self::create_context_server_in(this, id, configuration, root_path, cx).await
    }

    /// Builds the server for `configuration` with `root_path` as the project
    /// root: the one its approval was checked with (ADR-F42).
    async fn create_context_server_in(
        this: WeakEntity<Self>,
        id: ContextServerId,
        configuration: Arc<ContextServerConfiguration>,
        root_path: Option<Arc<Path>>,
        cx: &mut AsyncApp,
    ) -> Result<(Arc<ContextServer>, Arc<ContextServerConfiguration>)> {
        let remote = configuration.remote();
        let needs_remote_command = match configuration.as_ref() {
            ContextServerConfiguration::Custom { .. }
            | ContextServerConfiguration::Extension { .. } => remote,
            ContextServerConfiguration::Http { .. } => false,
        };

        let (remote_state, is_remote_project, project_defined) =
            this.update(cx, |this, _| {
                let project_defined = this
                    .context_server_settings
                    .get(&id.0)
                    .is_some_and(|entry| entry.source.is_project());
                if project_defined
                    && this
                        .approved_project_configurations
                        .get(&id)
                        .map(Arc::as_ref)
                        != Some(configuration.as_ref())
                {
                    anyhow::bail!(i18n::t!(
                        "MCP server \"{id}\" is defined in the project settings, and this definition has not been approved yet"
                    ));
                }
                let remote_state = match &this.state {
                    ContextServerStoreState::Remote {
                        project_id,
                        upstream_client,
                    } if needs_remote_command => Some((*project_id, upstream_client.clone())),
                    _ => None,
                };
                anyhow::Ok((remote_state, this.is_remote_project(), project_defined))
            })??;

        // `needs_remote_command` only says what the settings asked for; the
        // command is actually resolved over there when the store is also the
        // remote end of a connection. A `remote: true` server in a purely local
        // project still spawns on this machine.
        let runs_on_this_machine = remote_state.is_none();

        let configuration = if let Some((project_id, upstream_client)) = remote_state {
            // As on this machine, a server from the user's settings or an
            // extension does not start in the project over there either.
            let root_dir = root_path
                .as_ref()
                .filter(|_| project_defined)
                .map(|path| path.display().to_string());

            let response = upstream_client
                .update(cx, |client, _| {
                    client
                        .proto_client()
                        .request(proto::GetContextServerCommand {
                            project_id,
                            server_id: id.0.to_string(),
                            root_dir: root_dir.clone(),
                        })
                })
                .await?;

            // The remote host resolves the command from its own settings,
            // which commands run over there can rewrite, so what comes back
            // must be what was resolved (and approved) here, whoever defined
            // the server.
            match configuration.as_ref() {
                ContextServerConfiguration::Custom { command, .. } => {
                    verify_remote_command(command, &response)?;
                }
                ContextServerConfiguration::Extension { settings, .. } => {
                    Self::approve_remote_resolved_command(
                        this.clone(),
                        &id,
                        &response,
                        settings,
                        root_path.as_deref().filter(|_| project_defined),
                        cx,
                    )
                    .await?;
                }
                ContextServerConfiguration::Http { .. } => {}
            }

            let remote_command = upstream_client.update(cx, |client, _| {
                client.build_command(
                    Some(response.path),
                    &response.args,
                    &response.env.into_iter().collect(),
                    root_dir,
                    None,
                    Interactive::Yes,
                )
            })?;

            let command = ContextServerCommand {
                path: remote_command.program.into(),
                args: remote_command.args,
                env: Some(remote_command.env.into_iter().collect()),
                timeout: None,
            };

            Arc::new(ContextServerConfiguration::Custom { command, remote })
        } else {
            configuration
        };

        if let Some(server) = this.update(cx, |this, _| {
            this.context_server_factory
                .as_ref()
                .map(|factory| factory(id.clone(), configuration.clone()))
        })? {
            return Ok((server, configuration));
        }

        let working_directory = working_directory_for(
            &configuration,
            root_path,
            is_remote_project,
            project_defined,
        );
        if let Some(directory) = working_directory.clone()
            && directory == neutral_working_directory()
        {
            let fs = this.update(cx, |this, cx| this.worktree_store.read(cx).fs())?;
            create_neutral_working_directory(fs, directory, cx).await?;
        }

        let cached_token_provider: Option<Arc<dyn oauth::OAuthTokenProvider>> =
            if let ContextServerConfiguration::Http { url, .. } = configuration.as_ref() {
                if configuration.has_static_auth_header() {
                    None
                } else {
                    let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
                    let http_client = cx.update(|cx| cx.http_client());

                    match Self::load_session(&credentials_provider, url, &cx).await {
                        Ok(Some(session)) => {
                            log::info!("{} loaded cached OAuth session from keychain", id);
                            Some(Self::create_oauth_token_provider(
                                &id,
                                url,
                                session,
                                http_client,
                                credentials_provider,
                                cx,
                            ))
                        }
                        Ok(None) => None,
                        Err(err) => {
                            log::warn!("{} failed to load cached OAuth session: {}", id, err);
                            None
                        }
                    }
                }
            } else {
                None
            };

        let server: Arc<ContextServer> = this.update(cx, |this, cx| {
            let global_timeout = this.timeout_for_server(&id, cx);

            match configuration.as_ref() {
                ContextServerConfiguration::Http {
                    url,
                    headers,
                    timeout,
                    oauth: _,
                } => {
                    let transport = HttpTransport::new_with_token_provider(
                        cx.http_client(),
                        url.to_string(),
                        headers.clone(),
                        cx.background_executor().clone(),
                        cached_token_provider.clone(),
                    );
                    anyhow::Ok(Arc::new(ContextServer::new_with_timeout(
                        id,
                        Arc::new(transport),
                        Some(Duration::from_secs(
                            timeout.unwrap_or(global_timeout).min(MAX_TIMEOUT_SECS),
                        )),
                    )))
                }
                _ => {
                    let mut command = configuration
                        .command()
                        .context("Missing command configuration for stdio context server")?
                        .clone();
                    command.timeout = Some(
                        command
                            .timeout
                            .unwrap_or(global_timeout)
                            .min(MAX_TIMEOUT_SECS),
                    );

                    // Windows resolves a relative program against Zed's own
                    // working directory rather than the child's, while the
                    // approval key resolved it against the child's (ADR-F42).
                    if let Some(working_directory) = &working_directory
                        && command.path.is_relative()
                        && command.path.components().count() > 1
                    {
                        command.path = working_directory.join(&command.path);
                    }

                    // ADR-F13 (F2-7): point the MCP server at the fork's
                    // loopback egress proxy so its traffic is judged by the
                    // same allowlist the app's own requests are. Only when it
                    // runs on this machine — once `command` has been rewritten
                    // into the ssh/wsl wrapper its env belongs over there. A
                    // `remote: false` server in a remote project is spawned
                    // here, so it is covered like any local one.
                    if runs_on_this_machine && http_client::child_process_proxy().is_some() {
                        http_client::apply_child_process_proxy_env(
                            command.env.get_or_insert_with(Default::default),
                        );
                    }

                    anyhow::Ok(Arc::new(ContextServer::stdio(
                        id,
                        command,
                        working_directory,
                    )))
                }
            }
        })??;

        Ok((server, configuration))
    }

    async fn handle_get_context_server_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetContextServerCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::ContextServerCommand> {
        let server_id = ContextServerId(envelope.payload.server_id.into());

        let (settings_entry, registry, worktree_store) =
            this.update(&mut cx, |this, inner_cx| {
                let ContextServerStoreState::Local {
                    is_headless: true, ..
                } = &this.state
                else {
                    anyhow::bail!(
                        "unexpected GetContextServerCommand request in a non-local project"
                    );
                };

                let settings = this
                    .context_server_settings
                    .get(&server_id.0)
                    .cloned()
                    .or_else(|| {
                        this.registry
                            .read(inner_cx)
                            .context_server_descriptor(&server_id.0)
                            .map(|_| ContextServerSettingsEntry {
                                worktree_id: None,
                                settings: ContextServerSettings::default_extension(),
                                source: ContextServerSource::Extension,
                                replaces_user_entry: false,
                            })
                    })
                    .with_context(|| format!("context server `{}` not found", server_id))?;

                anyhow::Ok((settings, this.registry.clone(), this.worktree_store.clone()))
            })?;

        let configuration = ContextServerConfiguration::from_settings(
            settings_entry.settings,
            server_id.clone(),
            registry,
            worktree_store,
            &cx,
        )
        .await
        .with_context(|| format!("failed to build configuration for `{}`", server_id))?;

        let command = configuration
            .command()
            .context("context server has no command (HTTP servers don't need RPC)")?;

        // ADR-F14: this handler only ever runs in `remote_server` (the
        // `is_headless` check above), and the process it describes is spawned
        // on this host, so the endpoint to hand it is this host's — the one
        // the local injection in `create_context_server` deliberately withheld
        // from a remote server because a loopback port means a different
        // machine there. Applied to the resolved command's own environment, so
        // a proxy the user spelled out in `context_servers.<name>.env` still
        // wins.
        let mut env: HashMap<String, String> = command.env.clone().unwrap_or_default();
        if http_client::child_process_proxy().is_some() {
            http_client::apply_child_process_proxy_env(&mut env);
        }

        Ok(proto::ContextServerCommand {
            path: command.path.display().to_string(),
            args: command.args.clone(),
            env: env.into_iter().collect(),
        })
    }

    /// Merges context server settings from all visible worktrees so that servers defined
    /// in any project folder in a multi-root workspace are picked up.
    fn resolve_all_context_server_settings(
        worktree_store: &Entity<WorktreeStore>,
        launch_host: &LaunchHost,
        cx: &App,
    ) -> HashMap<Arc<str>, ContextServerSettingsEntry> {
        let mut merged = HashMap::default();
        let Some(settings_store) = cx.try_global::<SettingsStore>() else {
            return merged;
        };
        let worktrees = worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx))
            .collect::<Vec<_>>();
        for worktree in &worktrees {
            let worktree_id = worktree.id();
            let location = settings::SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            };
            for (id, settings) in &ProjectSettings::get(Some(location), cx).context_servers {
                merged.entry(id.clone()).or_insert_with(|| {
                    let (source, replaces_user_entry) = context_server_source(
                        id,
                        settings,
                        worktree,
                        &worktrees,
                        launch_host,
                        settings_store,
                    );
                    ContextServerSettingsEntry {
                        worktree_id: Some(worktree_id),
                        settings: settings.clone(),
                        source,
                        replaces_user_entry,
                    }
                });
            }
        }
        merged
    }

    fn create_oauth_token_provider(
        id: &ContextServerId,
        server_url: &url::Url,
        session: OAuthSession,
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut AsyncApp,
    ) -> Arc<dyn oauth::OAuthTokenProvider> {
        let (token_refresh_tx, mut token_refresh_rx) = futures::channel::mpsc::unbounded();
        let id = id.clone();
        let server_url = server_url.clone();

        cx.spawn(async move |cx| {
            while let Some(refreshed_session) = token_refresh_rx.next().await {
                if let Err(err) =
                    Self::store_session(&credentials_provider, &server_url, &refreshed_session, &cx)
                        .await
                {
                    log::warn!("{} failed to persist refreshed OAuth session: {}", id, err);
                }
            }
            log::debug!("{} OAuth session persistence task ended", id);
        })
        .detach();

        Arc::new(McpOAuthTokenProvider::new(
            session,
            http_client,
            Some(token_refresh_tx),
        ))
    }

    fn timeout_for_server(&self, id: &ContextServerId, cx: &App) -> u64 {
        let worktree_id = self
            .context_server_settings
            .get(&id.0)
            .as_ref()
            .and_then(|entry| entry.worktree_id);

        ProjectSettings::get(
            worktree_id.map(|id| SettingsLocation {
                worktree_id: id,
                path: &RelPath::empty(),
            }),
            cx,
        )
        .context_server_timeout
    }

    /// Initiate the OAuth browser flow for a server in the `AuthRequired` state.
    ///
    /// This starts a loopback HTTP callback server on an ephemeral port, builds
    /// the authorization URL, opens the user's browser, waits for the callback,
    /// exchanges the code for tokens, persists them in the keychain, and restarts
    /// the server with the new token provider.
    pub fn authenticate_server(
        &mut self,
        id: &ContextServerId,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let state = self.servers.get(id).context("Context server not found")?;
        let global_timeout = self.timeout_for_server(id, cx);

        let (discovery, server, configuration) = match state {
            ContextServerState::AuthRequired {
                discovery,
                server,
                configuration,
            } => (discovery.clone(), server.clone(), configuration.clone()),
            _ => anyhow::bail!("Server is not in AuthRequired state"),
        };

        let needs_keychain_check = match configuration.as_ref() {
            ContextServerConfiguration::Http {
                url,
                oauth: Some(oauth_settings),
                ..
            } if oauth_settings.client_secret.is_none() => Some(url.clone()),
            _ => None,
        };

        let id = id.clone();

        let task = cx.spawn({
            let id = id.clone();
            let server = server.clone();
            let configuration = configuration.clone();
            async move |this, cx| {
                if let Some(server_url) = needs_keychain_check {
                    let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
                    let has_keychain_secret =
                        Self::load_client_secret(&credentials_provider, &server_url, cx)
                            .await
                            .ok()
                            .flatten()
                            .is_some();

                    if !has_keychain_secret {
                        this.update(cx, |this, cx| {
                            this.update_server_state(
                                id.clone(),
                                ContextServerState::ClientSecretRequired {
                                    server,
                                    configuration,
                                    discovery,
                                    error: None,
                                },
                                cx,
                            );
                        })
                        .log_err();
                        return;
                    }
                }

                let result = Self::run_oauth_flow(
                    this.clone(),
                    id.clone(),
                    discovery.clone(),
                    configuration.clone(),
                    global_timeout,
                    cx,
                )
                .await;

                if let Err(err) = &result {
                    log::error!("{} OAuth authentication failed: {:?}", id, err);
                    this.update(cx, |this, cx| {
                        this.update_server_state(
                            id.clone(),
                            ContextServerState::Error {
                                server,
                                configuration,
                                error: format!("{err:#}").into(),
                            },
                            cx,
                        )
                    })
                    .log_err();
                }
            }
        });

        self.update_server_state(
            id,
            ContextServerState::Authenticating {
                server,
                configuration,
                _task: task,
            },
            cx,
        );

        Ok(())
    }

    /// Store the client secret and proceed with authentication.
    pub fn submit_client_secret(
        &mut self,
        id: &ContextServerId,
        secret: String,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let state = self.servers.get(id).context("Context server not found")?;
        let global_timeout = self.timeout_for_server(id, cx);

        let (server, configuration, discovery) = match state {
            ContextServerState::ClientSecretRequired {
                server,
                configuration,
                discovery,
                ..
            } => (server.clone(), configuration.clone(), discovery.clone()),
            _ => anyhow::bail!("Server is not in ClientSecretRequired state"),
        };

        let server_url = match configuration.as_ref() {
            ContextServerConfiguration::Http { url, .. } => url.clone(),
            _ => anyhow::bail!("OAuth only supported for HTTP servers"),
        };

        let id = id.clone();

        let task = cx.spawn({
            let id = id.clone();
            let server = server.clone();
            let configuration = configuration.clone();
            async move |this, cx| {
                // Store the secret if non-empty (empty means public client / skip).
                if !secret.is_empty() {
                    let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
                    if let Err(err) =
                        Self::store_client_secret(&credentials_provider, &server_url, &secret, cx)
                            .await
                    {
                        log::error!(
                            "{} failed to store client secret in keychain: {:?}",
                            id,
                            err
                        );
                    }
                }

                let result = Self::run_oauth_flow(
                    this.clone(),
                    id.clone(),
                    discovery.clone(),
                    configuration.clone(),
                    global_timeout,
                    cx,
                )
                .await;

                if let Err(err) = &result {
                    log::error!("{} OAuth authentication failed: {:?}", id, err);

                    let is_bad_client_credentials = err
                        .downcast_ref::<oauth::OAuthTokenError>()
                        .is_some_and(|e| e.error == "unauthorized_client");

                    if is_bad_client_credentials {
                        // Clear the bad secret from the keychain so the user
                        // gets a fresh prompt.
                        let credentials_provider =
                            cx.update(|cx| zed_credentials_provider::global(cx));
                        Self::clear_client_secret(&credentials_provider, &server_url, cx)
                            .await
                            .log_err();

                        this.update(cx, |this, cx| {
                            this.update_server_state(
                                id.clone(),
                                ContextServerState::ClientSecretRequired {
                                    server,
                                    configuration,
                                    discovery,
                                    error: Some(format!("{err:#}").into()),
                                },
                                cx,
                            );
                        })
                        .log_err();
                    } else {
                        this.update(cx, |this, cx| {
                            this.update_server_state(
                                id.clone(),
                                ContextServerState::Error {
                                    server,
                                    configuration,
                                    error: format!("{err:#}").into(),
                                },
                                cx,
                            )
                        })
                        .log_err();
                    }
                }
            }
        });

        self.update_server_state(
            id,
            ContextServerState::Authenticating {
                server,
                configuration,
                _task: task,
            },
            cx,
        );

        Ok(())
    }

    async fn run_oauth_flow(
        this: WeakEntity<Self>,
        id: ContextServerId,
        discovery: Arc<OAuthDiscovery>,
        configuration: Arc<ContextServerConfiguration>,
        global_timeout: u64,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let resource = oauth::canonical_server_uri(&discovery.resource_metadata.resource);
        let pkce = oauth::generate_pkce_challenge();

        let mut state_bytes = [0u8; 32];
        rand::rng().fill(&mut state_bytes);
        let state_param: String = state_bytes.iter().map(|b| format!("{:02x}", b)).collect();

        // Start a loopback HTTP server on an ephemeral port. The redirect URI
        // includes this port so the browser sends the callback directly to our
        // process.
        let (redirect_uri, callback_rx) =
            oauth::start_callback_server().context("Failed to start OAuth callback server")?;

        let http_client = cx.update(|cx| cx.http_client());
        let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
        let server_url = match configuration.as_ref() {
            ContextServerConfiguration::Http { url, .. } => url.clone(),
            _ => anyhow::bail!("OAuth authentication only supported for HTTP servers"),
        };

        let client_registration = match configuration.as_ref() {
            ContextServerConfiguration::Http {
                url,
                oauth: Some(oauth_settings),
                ..
            } => {
                // Pre-registered client. Resolve the secret from settings, then keychain.
                let client_secret = if oauth_settings.client_secret.is_some() {
                    oauth_settings.client_secret.clone()
                } else {
                    Self::load_client_secret(&credentials_provider, url, cx)
                        .await
                        .ok()
                        .flatten()
                };
                oauth::OAuthClientRegistration {
                    client_id: oauth_settings.client_id.clone(),
                    client_secret,
                }
            }
            _ => oauth::resolve_client_registration(&http_client, &discovery, &redirect_uri)
                .await
                .context("Failed to resolve OAuth client registration")?,
        };

        let auth_url = oauth::build_authorization_url(
            &discovery.auth_server_metadata,
            &client_registration.client_id,
            &redirect_uri,
            &discovery.scopes,
            &resource,
            &pkce,
            &state_param,
        );

        cx.update(|cx| cx.open_url(auth_url.as_str()));

        let callback = callback_rx
            .await
            .context("OAuth callback server received an invalid request")?;

        if callback.state != state_param {
            anyhow::bail!("OAuth state parameter mismatch (possible CSRF)");
        }

        let tokens = oauth::exchange_code(
            &http_client,
            &discovery.auth_server_metadata,
            &callback.code,
            &client_registration.client_id,
            &redirect_uri,
            &pkce.verifier,
            &resource,
            client_registration.client_secret.as_deref(),
        )
        .await
        .context("Failed to exchange authorization code for tokens")?;

        let session = OAuthSession {
            token_endpoint: discovery.auth_server_metadata.token_endpoint.clone(),
            resource: discovery.resource_metadata.resource.clone(),
            client_registration,
            tokens,
        };

        Self::store_session(&credentials_provider, &server_url, &session, cx)
            .await
            .context("Failed to persist OAuth session in keychain")?;

        let token_provider = Self::create_oauth_token_provider(
            &id,
            &server_url,
            session,
            http_client.clone(),
            credentials_provider,
            cx,
        );

        let new_server = this.update(cx, |_this, cx| match configuration.as_ref() {
            ContextServerConfiguration::Http {
                url,
                headers,
                timeout,
                oauth: _,
            } => {
                let transport = HttpTransport::new_with_token_provider(
                    http_client.clone(),
                    url.to_string(),
                    headers.clone(),
                    cx.background_executor().clone(),
                    Some(token_provider.clone()),
                );
                Ok(Arc::new(ContextServer::new_with_timeout(
                    id.clone(),
                    Arc::new(transport),
                    Some(Duration::from_secs(
                        timeout.unwrap_or(global_timeout).min(MAX_TIMEOUT_SECS),
                    )),
                )))
            }
            _ => anyhow::bail!("OAuth authentication only supported for HTTP servers"),
        })??;

        this.update(cx, |this, cx| {
            this.run_server(new_server, configuration, cx);
        })?;

        Ok(())
    }

    /// Store the full OAuth session in the system keychain, keyed by the
    /// server's canonical URI.
    async fn store_session(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        session: &OAuthSession,
        cx: &AsyncApp,
    ) -> Result<()> {
        let key = Self::keychain_key(server_url);
        let json = serde_json::to_string(session)?;
        credentials_provider
            .write_credentials(&key, "mcp-oauth", json.as_bytes(), cx)
            .await
    }

    /// Load the full OAuth session from the system keychain for the given
    /// server URL.
    async fn load_session(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        cx: &AsyncApp,
    ) -> Result<Option<OAuthSession>> {
        let key = Self::keychain_key(server_url);
        match credentials_provider.read_credentials(&key, cx).await? {
            Some((_username, password_bytes)) => {
                let session: OAuthSession = serde_json::from_slice(&password_bytes)?;
                Ok(Some(session))
            }
            None => Ok(None),
        }
    }

    /// Clear the stored OAuth session from the system keychain.
    async fn clear_session(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        cx: &AsyncApp,
    ) -> Result<()> {
        let key = Self::keychain_key(server_url);
        credentials_provider.delete_credentials(&key, cx).await
    }

    fn keychain_key(server_url: &url::Url) -> String {
        format!("mcp-oauth:{}", oauth::canonical_server_uri(server_url))
    }

    fn client_secret_keychain_key(server_url: &url::Url) -> String {
        format!(
            "mcp-oauth-client-secret:{}",
            oauth::canonical_server_uri(server_url)
        )
    }

    async fn load_client_secret(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        cx: &AsyncApp,
    ) -> Result<Option<String>> {
        let key = Self::client_secret_keychain_key(server_url);
        match credentials_provider.read_credentials(&key, cx).await? {
            Some((_username, secret_bytes)) => Ok(Some(String::from_utf8(secret_bytes)?)),
            None => Ok(None),
        }
    }

    pub async fn store_client_secret(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        secret: &str,
        cx: &AsyncApp,
    ) -> Result<()> {
        let key = Self::client_secret_keychain_key(server_url);
        credentials_provider
            .write_credentials(&key, "mcp-oauth-client-secret", secret.as_bytes(), cx)
            .await
    }

    async fn clear_client_secret(
        credentials_provider: &Arc<dyn CredentialsProvider>,
        server_url: &url::Url,
        cx: &AsyncApp,
    ) -> Result<()> {
        let key = Self::client_secret_keychain_key(server_url);
        credentials_provider.delete_credentials(&key, cx).await
    }

    /// Log out of an OAuth-authenticated MCP server: clear the stored OAuth
    /// session from the keychain and stop the server.
    pub fn logout_server(&mut self, id: &ContextServerId, cx: &mut Context<Self>) -> Result<()> {
        let state = self.servers.get(id).context("Context server not found")?;
        let configuration = state.configuration();

        let server_url = match configuration.as_ref() {
            ContextServerConfiguration::Http { url, .. } => url.clone(),
            _ => anyhow::bail!("logout only applies to HTTP servers with OAuth"),
        };

        let id = id.clone();
        self.stop_server(&id, cx)?;

        cx.spawn(async move |this, cx| {
            let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
            if let Err(err) = Self::clear_session(&credentials_provider, &server_url, &cx).await {
                log::error!("{} failed to clear OAuth session: {}", id, err);
            }
            // Also clear any client secret so the user gets a fresh prompt on
            // the next authentication attempt.
            Self::clear_client_secret(&credentials_provider, &server_url, &cx)
                .await
                .log_err();
            // Trigger server recreation so the next start uses a fresh
            // transport without the old (now-invalidated) token provider.
            this.update(cx, |this, cx| {
                this.available_context_servers_changed(cx);
            })
            .log_err();
        })
        .detach();

        Ok(())
    }

    fn update_server_state(
        &mut self,
        id: ContextServerId,
        state: ContextServerState,
        cx: &mut Context<Self>,
    ) {
        let status = ContextServerStatus::from_state(&state);
        self.servers.insert(id.clone(), state);
        cx.emit(ServerStatusChangedEvent {
            server_id: id,
            status,
        });
        cx.notify();
    }

    fn available_context_servers_changed(&mut self, cx: &mut Context<Self>) {
        if self.update_servers_task.is_some() {
            self.needs_server_update = true;
        } else {
            self.needs_server_update = false;
            self.update_servers_task = Some(cx.spawn(async move |this, cx| {
                if let Err(err) = Self::maintain_servers(this.clone(), cx).await {
                    log::error!("Error maintaining context servers: {}", err);
                }

                this.update(cx, |this, cx| {
                    this.populate_server_ids(cx);
                    cx.notify();
                    this.update_servers_task.take();
                    if this.needs_server_update {
                        this.available_context_servers_changed(cx);
                    }
                })?;

                Ok(())
            }));
        }
    }

    async fn maintain_servers(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        // Don't start context servers if AI is disabled
        let ai_disabled = this.update(cx, |_, cx| DisableAiSettings::get_global(cx).disable_ai)?;
        if ai_disabled {
            // Stop all running servers when AI is disabled
            this.update(cx, |this, cx| {
                let server_ids: Vec<_> = this.servers.keys().cloned().collect();
                for id in server_ids {
                    this.stop_server(&id, cx).log_err();
                }
            })?;
            return Ok(());
        }

        // One project root for the approval keys and the spawns alike.
        let (mut configured_servers, registry, worktree_store, launch_approvals, fs, root_path) =
            this.update(cx, |this, cx| {
                (
                    this.context_server_settings.clone(),
                    this.registry.clone(),
                    this.worktree_store.clone(),
                    this.launch_approvals.clone(),
                    this.worktree_store.read(cx).fs(),
                    this.resolve_root_path(cx),
                )
            })?;

        for (id, _) in registry.read_with(cx, |registry, _| registry.context_server_descriptors()) {
            configured_servers
                .entry(id)
                .or_insert(ContextServerSettingsEntry {
                    worktree_id: None,
                    settings: ContextServerSettings::default_extension(),
                    source: ContextServerSource::Extension,
                    replaces_user_entry: false,
                });
        }

        let (enabled_servers, disabled_servers): (HashMap<_, _>, HashMap<_, _>) =
            configured_servers
                .into_iter()
                .partition(|(_, entry)| entry.settings.enabled());

        let configured_servers =
            join_all(enabled_servers.into_iter().map(|(id, settings_entry)| {
                let id = ContextServerId(id);
                ContextServerConfiguration::from_settings(
                    settings_entry.settings.clone(),
                    id.clone(),
                    registry.clone(),
                    worktree_store.clone(),
                    cx,
                )
                .map(move |config| (id, config, settings_entry))
            }))
            .await
            .into_iter()
            .filter_map(|(id, config, settings_entry)| {
                config.map(|config| (id, (config, settings_entry)))
            })
            .collect::<HashMap<_, _>>();

        let mut project_launches = HashMap::default();
        let requester = this.entity_id();
        for (id, (configuration, settings_entry)) in &configured_servers {
            let request = this.update(cx, |this, cx| {
                this.context_server_launch_request(
                    id,
                    configuration,
                    settings_entry,
                    root_path.clone(),
                    cx,
                )
            })?;
            let Some(request) = request else {
                continue;
            };
            let details = request.resolve(fs.clone()).await;
            let details = match &launch_approvals {
                Some(launch_approvals) => launch_approvals.update(cx, |launch_approvals, cx| {
                    launch_approvals.request_launch(details, requester, cx)
                }),
                None => details,
            };
            project_launches.insert(id.clone(), details);
        }

        // A project-defined server is launched only with an approval of
        // exactly what it would run (ADR-F42).
        let withheld_servers = configured_servers
            .iter()
            .filter(|(id, (_, settings_entry))| {
                settings_entry.source.is_project()
                    && !project_launches
                        .get(*id)
                        .is_some_and(|details: &LaunchApprovalDetails| {
                            details.state == LaunchApprovalState::Approved
                        })
            })
            .map(|(id, _)| id.clone())
            .collect::<HashSet<_>>();

        let mut servers_to_start = Vec::new();
        let mut servers_to_remove = HashSet::default();
        let mut servers_to_stop = HashSet::default();
        let mut servers_to_withhold = HashSet::default();

        this.update(cx, |this, _| {
            for server_id in this.servers.keys() {
                // All servers that are not in desired_servers should be removed from the store.
                // This can happen if the user removed a server from the context server settings.
                if !configured_servers.contains_key(server_id) {
                    if disabled_servers.contains_key(&server_id.0) {
                        servers_to_stop.insert(server_id.clone());
                    } else {
                        servers_to_remove.insert(server_id.clone());
                    }
                } else if withheld_servers.contains(server_id) {
                    servers_to_withhold.insert(server_id.clone());
                }
            }

            let is_remote_project = this.is_remote_project();
            let mut approved_project_configurations = HashMap::default();
            this.remote_resolved_launches.retain(|id, _| {
                is_remote_project
                    && configured_servers
                        .get(id)
                        .is_some_and(|(configuration, _)| {
                            matches!(
                                configuration,
                                ContextServerConfiguration::Extension { remote: true, .. }
                            )
                        })
            });

            for (id, (config, settings_entry)) in configured_servers {
                if withheld_servers.contains(&id) {
                    continue;
                }
                let state = this.servers.get(&id);
                let is_stopped = matches!(state, Some(ContextServerState::Stopped { .. }));
                // A `remote: true` server runs the command wrapped for the
                // remote host, which never equals the settings.
                let existing_config = this
                    .started_configurations
                    .get(&id)
                    .cloned()
                    .or_else(|| state.as_ref().map(|state| state.configuration()));
                let working_directory = working_directory_for(
                    &config,
                    root_path.clone(),
                    is_remote_project,
                    settings_entry.source.is_project(),
                );
                // A running server that was started before the project root became
                // available keeps its stale working directory, since the working
                // directory is not part of `ContextServerConfiguration`. Restart it
                // when the resolved working directory no longer matches.
                let working_directory_changed = state.is_some()
                    && !is_stopped
                    && this.server_working_directories.get(&id) != Some(&working_directory);
                let config = Arc::new(config);
                if settings_entry.source.is_project() {
                    approved_project_configurations.insert(id.clone(), config.clone());
                }
                if existing_config.as_deref() != Some(config.as_ref())
                    || is_stopped
                    || working_directory_changed
                {
                    servers_to_start.push((id.clone(), config, working_directory));
                    if this.servers.contains_key(&id) {
                        servers_to_stop.insert(id);
                    }
                }
            }
            this.approved_project_configurations = approved_project_configurations;

            anyhow::Ok(())
        })??;

        let starting_servers = servers_to_start
            .iter()
            .map(|(id, _, _)| id.clone())
            .collect::<HashSet<_>>();
        this.update(cx, |this, inner_cx| {
            for id in servers_to_stop {
                this.stop_server(&id, inner_cx)?;
            }
            for id in servers_to_remove {
                this.remove_server(&id, inner_cx)?;
            }
            for id in &servers_to_withhold {
                this.withhold_server(id);
            }
            this.update_project_launches(
                project_launches,
                &servers_to_withhold,
                &starting_servers,
                inner_cx,
            );
            anyhow::Ok(())
        })??;

        for (id, config, working_directory) in servers_to_start {
            match Self::create_context_server_in(
                this.clone(),
                id.clone(),
                config.clone(),
                root_path.clone(),
                cx,
            )
            .await
            {
                Ok((server, started_config)) => {
                    this.update(cx, |this, cx| {
                        // A server started while this one was being built,
                        // from the same definition in the same directory (as
                        // `start_server` does with a server of its own), is
                        // left running.
                        let started_meanwhile = matches!(
                            this.servers.get(&id),
                            Some(
                                ContextServerState::Starting { .. }
                                    | ContextServerState::Running { .. }
                                    | ContextServerState::Authenticating { .. }
                            )
                        ) && this.started_configurations.get(&id)
                            == Some(&config)
                            && this.server_working_directories.get(&id) == Some(&working_directory);
                        if started_meanwhile {
                            return;
                        }
                        this.server_working_directories
                            .insert(id.clone(), working_directory);
                        this.started_configurations.insert(id.clone(), config);
                        this.run_server(server, started_config, cx);
                    })?;
                }
                Err(err) if err.is::<LaunchNeedsApproval>() => {
                    this.update(cx, |_this, cx| {
                        cx.emit(ServerStatusChangedEvent {
                            server_id: id,
                            status: ContextServerStatus::ApprovalRequired,
                        });
                        cx.notify();
                    })?;
                }
                Err(err) => {
                    log::error!("{id} context server failed to create: {err:#}");
                    this.update(cx, |_this, cx| {
                        cx.emit(ServerStatusChangedEvent {
                            server_id: id,
                            status: ContextServerStatus::Error(err.to_string().into()),
                        });
                        cx.notify();
                    })?;
                }
            }
        }

        Ok(())
    }
}

/// How long [`wait_for_context_server`] waits for a server to settle before
/// giving up. OAuth flows go through the browser, so this is generous.
const WAIT_FOR_SERVER_TIMEOUT: Duration = Duration::from_secs(120);

/// The outcome of waiting for a server to settle: either the terminal status it
/// reached, or a message describing why it never got there.
type WaitOutcome = Result<ContextServerStatus, Arc<str>>;

/// Resolves once `context_server_id` reaches a status that ends a (re)start
/// attempt — running, or waiting on the user for authorization — or with a
/// message to show when it stops, errors, or never settles.
///
/// Callers that restart a server must stop it *before* calling this, otherwise
/// the `Stopped` event from their own stop resolves the wait immediately.
///
/// Lives here rather than in a UI crate because both the agent panel's
/// "configure MCP server" modal and the settings window's MCP page wait the
/// same way; the messages are user-facing, hence Japanese (ADR-F07).
pub fn wait_for_context_server(
    context_server_store: &Entity<ContextServerStore>,
    context_server_id: ContextServerId,
    cx: &mut App,
) -> Task<WaitOutcome> {
    if context_server_store
        .read(cx)
        .status_for_server(&context_server_id)
        == Some(ContextServerStatus::ApprovalRequired)
    {
        return Task::ready(Err(approval_required_message(&context_server_id)));
    }

    let (tx, rx) = futures::channel::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let context_server_id_for_timeout = context_server_id.clone();
    let subscription = cx.subscribe(context_server_store, move |_, event, _cx| {
        let ServerStatusChangedEvent { server_id, status } = event;

        if server_id != &context_server_id {
            return;
        }

        match status {
            ContextServerStatus::Running
            | ContextServerStatus::AuthRequired
            | ContextServerStatus::ClientSecretRequired { .. } => {
                send_wait_outcome(&tx, Ok(status.clone()));
            }
            ContextServerStatus::Stopped => {
                send_wait_outcome(&tx, Err(i18n::t!("The MCP server stopped").into()));
            }
            ContextServerStatus::Error(error) => {
                send_wait_outcome(&tx, Err(error.clone()));
            }
            ContextServerStatus::ApprovalRequired => {
                send_wait_outcome(&tx, Err(approval_required_message(&context_server_id)));
            }
            ContextServerStatus::Starting | ContextServerStatus::Authenticating => {}
        }
    });

    cx.spawn(async move |cx| {
        let timeout = cx.background_executor().timer(WAIT_FOR_SERVER_TIMEOUT);
        let result = futures::future::select(rx, timeout).await;
        drop(subscription);
        match result {
            Either::Left((Ok(inner), _)) => inner,
            Either::Left((Err(_), _)) => {
                Err(Arc::from(i18n::t!("The MCP server store was dropped")))
            }
            Either::Right(_) => Err(Arc::from(i18n::t!(
                "Starting MCP server `{id}` timed out. See the Zed log for details.",
                id = context_server_id_for_timeout
            ))),
        }
    })
}

fn send_wait_outcome(
    tx: &Mutex<Option<futures::channel::oneshot::Sender<WaitOutcome>>>,
    outcome: WaitOutcome,
) {
    let Some(tx) = tx.lock().take() else {
        return;
    };
    if tx.send(outcome).is_err() {
        log::debug!("context server wait was dropped before its outcome could be reported");
    }
}

fn approval_required_message(id: &ContextServerId) -> Arc<str> {
    i18n::t!(
        "MCP server \"{id}\" is waiting for approval. A server defined in the project settings \
         does not start until what it runs is checked and allowed on the \"MCP Servers\" page of \
         the settings."
    )
    .into()
}

fn no_project_launch_error(id: &ContextServerId) -> anyhow::Error {
    anyhow!(i18n::t!(
        "MCP server \"{id}\" has no project definition that an approval could apply to"
    ))
}

/// Only a definition in a file that a sandboxed command cannot write — the
/// user's or the global settings — counts as the user's own. The effective
/// entry comes from the first worktree that has one, but an extension's
/// server reads its settings from every worktree, so for those a definition
/// in any worktree makes the server project-defined.
fn context_server_source(
    id: &str,
    settings: &ContextServerSettings,
    worktree: &Worktree,
    worktrees: &[&Worktree],
    launch_host: &LaunchHost,
    settings_store: &SettingsStore,
) -> (ContextServerSource, bool) {
    let defined_in = |file: SettingsFile| {
        settings_store
            .get_content_for_file(file)
            .is_some_and(|content| content.project.context_servers.contains_key(id))
    };
    let defines = |worktree: &Worktree| {
        defined_in(SettingsFile::Project((worktree.id(), RelPath::empty_arc())))
    };
    let defining_worktree = if matches!(settings, ContextServerSettings::Extension { .. }) {
        worktrees.iter().copied().find(|worktree| defines(worktree))
    } else {
        Some(worktree).filter(|worktree| defines(worktree))
    };
    let (worktree_id, settings_file) = match defining_worktree {
        Some(defining_worktree) => (
            defining_worktree.id(),
            worktree_settings_file(defining_worktree, RelPath::empty()),
        ),
        None if defined_in(SettingsFile::Server) => {
            (worktree.id(), LaunchDefinitionFile::RemoteServerSettings)
        }
        None => return (ContextServerSource::User, false),
    };
    (
        ContextServerSource::Project {
            worktree_id,
            settings_file,
            host: launch_host.clone(),
        },
        defined_in(SettingsFile::User) || defined_in(SettingsFile::Global),
    )
}

/// A launch held back until the user approves it (ADR-F42).
#[derive(Debug)]
struct LaunchNeedsApproval;

impl std::fmt::Display for LaunchNeedsApproval {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(i18n::t!("Starting this MCP server needs approval"))
    }
}

impl std::error::Error for LaunchNeedsApproval {}

/// The remote side resolves the command from its own copy of the settings,
/// which can have changed since the definition was approved here.
fn verify_remote_command(
    approved: &ContextServerCommand,
    response: &proto::ContextServerCommand,
) -> Result<()> {
    let approved_value = |name: &str| approved.env.as_ref().and_then(|env| env.get(name));
    let environment_matches = approved
        .env
        .iter()
        .flatten()
        .all(|(name, value)| response.env.get(name) == Some(value))
        && response
            .env
            .keys()
            .all(|name| approved_value(name).is_some() || is_child_process_proxy_variable(name));
    anyhow::ensure!(
        response.path == approved.path.to_string_lossy()
            && response.args == approved.args
            && environment_matches,
        i18n::t!(
            "The MCP server command the remote host returned does not match the approved definition"
        )
    );
    Ok(())
}

/// The remote side adds its own egress proxy to the environment (ADR-F14).
fn is_child_process_proxy_variable(name: &str) -> bool {
    let mut probe = HashMap::default();
    probe.insert(name.to_string(), String::new());
    http_client::env_overrides_child_process_proxy(&probe)
}

/// The working directory a server will be spawned with, mirroring the choice
/// made in [`ContextServerStore::create_context_server`].
fn working_directory_for(
    configuration: &ContextServerConfiguration,
    root_path: Option<Arc<Path>>,
    is_remote_project: bool,
    project_defined: bool,
) -> Option<Arc<Path>> {
    match configuration {
        ContextServerConfiguration::Http { .. } => None,
        _ => local_working_directory(is_remote_project, project_defined, root_path),
    }
}

/// Only an approved project-defined server starts in the project. What an
/// interpreter or package runner loads from its working directory is part of
/// no approval, so every other stdio server spawned here — from the user's
/// settings or an extension, and any in a remote project, whose paths are no
/// working directory for a process here — starts in the neutral directory,
/// where an agent cannot plant code for it to pick up (ADR-F42).
fn local_working_directory(
    is_remote_project: bool,
    project_defined: bool,
    root_path: Option<Arc<Path>>,
) -> Option<Arc<Path>> {
    match root_path {
        Some(root_path) if project_defined && !is_remote_project => Some(root_path),
        _ => Some(neutral_working_directory()),
    }
}

const NEUTRAL_WORKING_DIRECTORY_NAME: &str = "mcp-neutral-cwd";

/// The working directory of a stdio server that does not start in the
/// project: `<data_dir>/mcp-neutral-cwd`, an empty directory of Zed's own in
/// the data directory the agent sandbox protects, created when a server is
/// spawned there.
pub fn neutral_working_directory() -> Arc<Path> {
    Arc::from(paths::data_dir().join(NEUTRAL_WORKING_DIRECTORY_NAME))
}

/// Through the project's file system where there is one (a local project).
async fn create_neutral_working_directory(
    fs: Option<Arc<dyn fs::Fs>>,
    directory: Arc<Path>,
    cx: &AsyncApp,
) -> Result<()> {
    let created = match fs {
        Some(fs) => fs.create_dir(&directory).await,
        None => {
            let directory = directory.clone();
            cx.background_executor()
                .spawn(
                    async move { std::fs::create_dir_all(&directory).map_err(anyhow::Error::from) },
                )
                .await
        }
    };
    created.with_context(|| {
        i18n::t!(
            "Could not create the working directory {directory} of the MCP server",
            directory = directory.display()
        )
    })
}

/// Determines the appropriate server state after a start attempt fails.
///
/// When the error is an HTTP 401 with no static auth header configured,
/// attempts OAuth discovery so the UI can offer an authentication flow.
async fn resolve_start_failure(
    id: &ContextServerId,
    err: anyhow::Error,
    server: Arc<ContextServer>,
    configuration: Arc<ContextServerConfiguration>,
    cx: &AsyncApp,
) -> ContextServerState {
    // Read the challenge from the transport rather than downcasting `err`: it
    // is recorded before the failed send's error propagates, so a 401 is
    // recognized even when another error (e.g. the request timeout) wins the
    // race to become the reported startup failure.
    let www_authenticate = server.auth_challenge();

    // When the error is NOT a 401 but there is a cached OAuth session in the
    // keychain, the session is likely stale/expired and caused the failure
    // (e.g. timeout because the server rejected the token silently). Clear it
    // so the next start attempt can get a clean 401 and trigger the auth flow.
    // If there is no such session this is an ordinary startup error.
    if www_authenticate.is_none() {
        let server_url = match configuration.as_ref() {
            ContextServerConfiguration::Http { url, .. }
                if !configuration.has_static_auth_header() =>
            {
                url.clone()
            }
            _ => {
                log::error!("{id} context server failed to start: {err}");
                return ContextServerState::Error {
                    configuration,
                    server,
                    error: err.to_string().into(),
                };
            }
        };

        let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
        match ContextServerStore::load_session(&credentials_provider, &server_url, cx).await {
            Ok(Some(_)) => {
                log::info!("{id} start failed with a cached OAuth session present; clearing it");
                ContextServerStore::clear_session(&credentials_provider, &server_url, cx)
                    .await
                    .log_err();
            }
            _ => {
                log::error!("{id} context server failed to start: {err}");
                return ContextServerState::Error {
                    configuration,
                    server,
                    error: err.to_string().into(),
                };
            }
        }
    }

    let default_www_authenticate = oauth::WwwAuthenticate {
        resource_metadata: None,
        scope: None,
        error: None,
        error_description: None,
    };
    let www_authenticate = www_authenticate
        .as_ref()
        .unwrap_or(&default_www_authenticate);

    resolve_auth_required(id, www_authenticate, server, configuration, cx).await
}

/// Runs OAuth discovery for a server that returned a 401 and produces the
/// appropriate state (`AuthRequired`, `ClientSecretRequired`, or `Error`).
///
/// Shared by the startup path ([`resolve_start_failure`]) and the
/// post-initialize path ([`ContextServerStore::handle_auth_challenge`]) so
/// that a 401 at any point — not only during `initialize` — can initiate the
/// OAuth flow.
async fn resolve_auth_required(
    id: &ContextServerId,
    www_authenticate: &oauth::WwwAuthenticate,
    server: Arc<ContextServer>,
    configuration: Arc<ContextServerConfiguration>,
    cx: &AsyncApp,
) -> ContextServerState {
    if configuration.has_static_auth_header() {
        log::warn!("{id} received 401 with a static Authorization header configured");
        return ContextServerState::Error {
            configuration,
            server,
            error: "Server returned 401 Unauthorized. Check your configured Authorization header."
                .into(),
        };
    }

    let server_url = match configuration.as_ref() {
        ContextServerConfiguration::Http { url, .. } => url.clone(),
        _ => {
            log::error!("{id} got OAuth 401 on a non-HTTP transport");
            return ContextServerState::Error {
                configuration,
                server,
                error: "Server returned 401 Unauthorized on a non-HTTP transport".into(),
            };
        }
    };

    let http_client = cx.update(|cx| cx.http_client());

    match context_server::oauth::discover(&http_client, &server_url, www_authenticate).await {
        Ok(discovery) => {
            use context_server::oauth::{
                ClientRegistrationStrategy, determine_registration_strategy,
            };

            let has_preregistered_client_id = matches!(
                configuration.as_ref(),
                ContextServerConfiguration::Http { oauth: Some(_), .. }
            );

            let strategy = determine_registration_strategy(&discovery.auth_server_metadata);

            if matches!(strategy, ClientRegistrationStrategy::Unavailable)
                && !has_preregistered_client_id
            {
                log::error!(
                    "{id} authorization server supports neither CIMD nor DCR, \
                     and no pre-registered client_id is configured"
                );
                return ContextServerState::Error {
                    configuration,
                    server,
                    error: "Authorization server supports neither CIMD nor DCR. \
                            Configure a pre-registered client_id in your settings \
                            under the \"oauth\" key."
                        .into(),
                };
            }

            log::info!(
                "{id} requires OAuth authorization (auth server: {})",
                discovery.auth_server_metadata.issuer,
            );
            ContextServerState::AuthRequired {
                server,
                configuration,
                discovery: Arc::new(discovery),
            }
        }
        Err(discovery_err) => {
            log::error!("{id} OAuth discovery failed: {discovery_err}");
            ContextServerState::Error {
                configuration,
                server,
                error: format!("OAuth discovery failed: {discovery_err}").into(),
            }
        }
    }
}
