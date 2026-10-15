use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use fs::{Fs, RemoveOptions};
use futures::StreamExt;
use gpui::{
    AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    TaskExt,
};
use http_client::{HttpClient, github::AssetKind};
use node_runtime::NodeRuntime;
use percent_encoding::percent_decode_str;
use remote::RemoteClient;
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{AgentConfigOptionValue, RegisterSetting, SettingsStore, update_settings_file};
use sha2::{Digest, Sha256};
use url::Url;
use util::{ResultExt as _, debug_panic};

use crate::ProjectEnvironment;
use crate::agent_registry_store::{AgentRegistryStore, RegistryAgent, RegistryTargetConfig};
#[cfg(target_os = "windows")]
use crate::agent_sandbox::WslAgentRuntime;
use crate::agent_sandbox::{
    AgentSandboxCommand, AgentSandboxHosts, AgentSandboxState, SandboxedAgentProfile,
    readable_paths_in_data_dir,
};
use crate::agent_update::{
    AgentInstallPlace, AgentUpdateApprovals, INSTALLED_RECORD_FILE_NAME, InstalledAgentLaunch,
    InstalledAgentRecord, LaunchDecision, UpdateApprovalClaim, host_npx_install_dir,
    launch_decision, local_agent_update_approvals, read_installed_record, record_start,
    write_installed_record,
};

use crate::worktree_store::WorktreeStore;

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, JsonSchema)]
pub struct AgentServerCommand {
    #[serde(rename = "command")]
    pub path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
    /// ADR-F49 第 2a 段: the command's part in the sandbox. `None` for one
    /// that says nothing about it (a remote host's too old to, a test's).
    #[serde(skip)]
    pub sandbox: Option<AgentSandboxCommand>,
}

impl std::fmt::Debug for AgentServerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filtered_env = self.env.as_ref().map(|env| {
            env.iter()
                .map(|(k, v)| {
                    (
                        k,
                        if util::redact::should_redact(k) {
                            "[REDACTED]"
                        } else {
                            v
                        },
                    )
                })
                .collect::<Vec<_>>()
        });

        f.debug_struct("AgentServerCommand")
            .field("path", &self.path)
            .field("args", &self.args)
            .field("env", &filtered_env)
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

/// ADR-F49 第 2b 段: where the command an agent server resolves is to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentLaunchTarget {
    /// This machine's sandbox: Bubblewrap on Linux, Seatbelt on macOS, and
    /// on Windows the WSL distro the project's folders are in (`None`: the
    /// default distro), where the command is a Linux command.
    Sandbox { wsl_distro: Option<String> },
    /// The host itself, outside any sandbox: the one start the user
    /// confirmed (ADR-F49 decision 19), and a remote host resolving for its
    /// own helper.
    Host,
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct AgentId(pub SharedString);

impl AgentId {
    pub fn new(id: impl Into<SharedString>) -> Self {
        AgentId(id.into())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&'static str> for AgentId {
    fn from(value: &'static str) -> Self {
        AgentId(value.into())
    }
}

impl From<AgentId> for SharedString {
    fn from(value: AgentId) -> Self {
        value.0
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExternalAgentSource {
    #[default]
    Custom,
    Registry,
}

pub trait ExternalAgentServer {
    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>>;

    /// ADR-F99: the command for something other than the agent's own start (a
    /// terminal sign-in), which never installs an update the user agreed to:
    /// that is the agent's next real start's to do.
    fn get_command_without_update(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        self.get_command(extra_args, extra_env, target, cx)
    }

    fn version(&self) -> Option<&SharedString> {
        None
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        None
    }

    fn set_new_version_available_tx(&mut self, _tx: watch::Sender<Option<String>>) {}

    fn take_loading_status_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        None
    }

    fn set_loading_status_tx(&mut self, _tx: watch::Sender<Option<String>>) {}

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

enum AgentServerStoreState {
    Local {
        node_runtime: NodeRuntime,
        fs: Arc<dyn Fs>,
        project_environment: Entity<ProjectEnvironment>,
        downstream_client: Option<(u64, AnyProtoClient)>,
        settings: Option<AllAgentServersSettings>,
        http_client: Arc<dyn HttpClient>,
        _subscriptions: Vec<Subscription>,
    },
    Remote {
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
        worktree_store: Entity<WorktreeStore>,
    },
    Collab,
}

pub struct ExternalAgentEntry {
    server: Box<dyn ExternalAgentServer>,
    icon: Option<SharedString>,
    display_name: Option<SharedString>,
    pub source: ExternalAgentSource,
}

impl ExternalAgentEntry {
    pub fn new(
        server: Box<dyn ExternalAgentServer>,
        source: ExternalAgentSource,
        icon: Option<SharedString>,
        display_name: Option<SharedString>,
    ) -> Self {
        Self {
            server,
            icon,
            display_name,
            source,
        }
    }
}

pub struct AgentServerStore {
    state: AgentServerStoreState,
    pub external_agents: HashMap<AgentId, ExternalAgentEntry>,
    /// ADR-F99: the agents whose update the user agreed to, which the next
    /// start of each installs.
    update_approvals: AgentUpdateApprovals,
}

/// ADR-F99: where a start reports a registry version newer than the one it
/// launched. Shared with the start's task, which only knows once it has read
/// the install's record, while the store keeps the sender for the registry
/// changes it sees itself.
type NewVersionSlot = Arc<parking_lot::Mutex<Option<watch::Sender<Option<String>>>>>;

fn report_new_version(slot: &NewVersionSlot, version: String) {
    if let Some(tx) = slot.lock().as_mut() {
        tx.send(Some(version)).ok();
    }
}

pub struct AgentServersUpdated;

impl EventEmitter<AgentServersUpdated> for AgentServerStore {}

static EXTENSION_TO_REGISTRY_IDS: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from_iter([
            ("opencode", "opencode"),
            ("mistral-vibe", "mistral-vibe"),
            ("auggie", "auggie"),
            ("stakpak", "stakpak"),
            ("codebuddy", "codebuddy-code"),
            ("autohand-acp", "autohand"),
            ("corust-agent", "corust-agent"),
            ("factory-droid", "factory-droid"),
            // Unmaintained
            // ("qqcode", ""),
        ])
    });

/// The id an agent written down under `id` in `agent_servers` is started by:
/// its registry id when `id` is the old extension one.
pub(crate) fn registry_id_for(id: &str) -> &str {
    EXTENSION_TO_REGISTRY_IDS.get(id).copied().unwrap_or(id)
}

impl AgentServerStore {
    pub fn migrate_agent_server_from_extensions(
        &mut self,
        id: Arc<str>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) {
        let Some(registry_id) = EXTENSION_TO_REGISTRY_IDS.get(id.as_ref()) else {
            return;
        };

        update_settings_file(fs, cx, move |settings, _| {
            let agent_servers = settings.agent_servers.get_or_insert_default();
            // Take the old settings
            let settings = agent_servers.remove(id.as_ref());
            // If they had both installed, just remove the extension settings, leave theirregistry settings alone
            if agent_servers.contains_key(*registry_id) {
                return;
            }
            // Insert the old settings, or write new ones so it is "installed" via the registry
            agent_servers.insert(
                registry_id.to_string(),
                settings.unwrap_or_else(|| settings::CustomAgentServerSettings::Registry {
                    default_mode: None,
                    env: Default::default(),
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                    allow_subscription_sign_in: None,
                    run_outside_sandbox: None,
                }),
            );
        });
    }

    pub fn agent_icon(&self, id: &AgentId) -> Option<SharedString> {
        self.external_agents
            .get(id)
            .and_then(|entry| entry.icon.clone())
    }

    pub fn agent_source(&self, name: &AgentId) -> Option<ExternalAgentSource> {
        self.external_agents.get(name).map(|entry| entry.source)
    }

    /// ADR-F99: the user agreed to update `agent_id` where `place` installs
    /// it. Its next start there installs the registry's version instead of
    /// launching the one installed; a remote host's store keeps its agreements
    /// as [`AgentInstallPlace::Host`], and tells the host with that start's
    /// request.
    pub fn approve_agent_update(&self, agent_id: &AgentId, place: AgentInstallPlace) {
        self.update_approvals.approve(agent_id, place);
    }

    pub fn agent_update_approved(&self, agent_id: &AgentId, place: &AgentInstallPlace) -> bool {
        self.update_approvals.is_approved(agent_id, place)
    }
}

impl AgentServerStore {
    pub fn agent_display_name(&self, name: &AgentId) -> Option<SharedString> {
        self.external_agents
            .get(name)
            .and_then(|entry| entry.display_name.clone())
    }

    pub fn init_remote(session: &AnyProtoClient) {
        session.add_entity_message_handler(Self::handle_external_agents_updated);
        session.add_entity_message_handler(Self::handle_loading_status_updated);
        session.add_entity_message_handler(Self::handle_new_version_available);
    }

    pub fn init_headless(session: &AnyProtoClient) {
        session.add_entity_request_handler(Self::handle_get_agent_server_command);
        session.add_entity_request_handler(Self::handle_list_sandbox_processes);
        session.add_entity_request_handler(Self::handle_stop_sandbox_processes);
    }

    /// ADR-F49 decision 18 (C): what still runs in a sandbox this host built
    /// for an agent. Answered from outside that sandbox — the helper's parent
    /// registered which PID namespace belongs to which session id, in a
    /// directory the sandbox cannot write — so nothing inside it can shape the
    /// answer.
    async fn handle_list_sandbox_processes(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::ListSandboxProcesses>,
        cx: AsyncApp,
    ) -> Result<proto::ListSandboxProcessesResponse> {
        let session_id = envelope.payload.session_id;
        let processes = cx
            .background_spawn(async move { list_sandbox_processes_on_this_host(&session_id) })
            .await?;
        Ok(proto::ListSandboxProcessesResponse {
            processes: processes
                .into_iter()
                .map(|process| proto::SandboxProcess {
                    pid: process.pid,
                    start_time: process.start_time,
                    running_for: process.running_for.map(|running_for| running_for.as_secs()),
                    command: process.command,
                })
                .collect(),
        })
    }

    /// ADR-F49 decision 18 (C): stops exactly the processes the client names,
    /// and only where the pid still carries the start time the client saw.
    async fn handle_stop_sandbox_processes(
        _this: Entity<Self>,
        envelope: TypedEnvelope<proto::StopSandboxProcesses>,
        cx: AsyncApp,
    ) -> Result<proto::StopSandboxProcessesResponse> {
        let session_id = envelope.payload.session_id;
        let processes = sandbox_process_keys(envelope.payload.processes)?;
        let stopped = cx
            .background_spawn(async move {
                stop_sandbox_processes_on_this_host(&session_id, &processes)
            })
            .await?;
        Ok(proto::StopSandboxProcessesResponse {
            stopped: stopped as u32,
        })
    }

    fn agent_servers_settings_changed(&mut self, cx: &mut Context<Self>) {
        let AgentServerStoreState::Local {
            settings: old_settings,
            ..
        } = &mut self.state
        else {
            debug_panic!(
                "should not be subscribed to agent server settings changes in non-local project"
            );
            return;
        };

        let new_settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None)
            .clone();
        if Some(&new_settings) == old_settings.as_ref() {
            return;
        }

        self.reregister_agents(cx);
    }

    fn reregister_agents(&mut self, cx: &mut Context<Self>) {
        let AgentServerStoreState::Local {
            node_runtime,
            fs,
            project_environment,
            downstream_client,
            settings: old_settings,
            http_client,
            ..
        } = &mut self.state
        else {
            debug_panic!("Non-local projects should never attempt to reregister. This is a bug!");

            return;
        };

        let new_settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None)
            .clone();

        // If we don't have agents from the registry loaded yet, trigger a
        // refresh, which will cause this function to be called again
        let registry_store = AgentRegistryStore::try_global(cx);
        if new_settings.has_registry_agents()
            && let Some(registry) = registry_store.as_ref()
        {
            registry.update(cx, |registry, cx| registry.refresh_if_stale(cx));
        }

        let update_approvals = self.update_approvals.clone();
        let registry_agents_by_id = registry_store
            .as_ref()
            .map(|store| {
                store
                    .read(cx)
                    .agents()
                    .iter()
                    .cloned()
                    .map(|agent| (agent.id().to_string(), agent))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        // Drain the existing versioned agents, extracting reconnect state
        // from any active connection so we can preserve it or trigger a
        // reconnect when the version changes.
        let mut old_versioned_agents: HashMap<
            AgentId,
            (
                SharedString,
                Option<watch::Sender<Option<String>>>,
                Option<watch::Sender<Option<String>>>,
            ),
        > = HashMap::default();
        for (name, mut entry) in self.external_agents.drain() {
            if let Some(version) = entry.server.version().cloned() {
                let new_version_available_tx = entry.server.take_new_version_available_tx();
                let loading_status_tx = entry.server.take_loading_status_tx();
                if new_version_available_tx.is_some() || loading_status_tx.is_some() {
                    old_versioned_agents
                        .insert(name, (version, new_version_available_tx, loading_status_tx));
                }
            }
        }

        for (name, settings) in new_settings.iter() {
            match settings {
                CustomAgentServerSettings::Custom { command, .. } => {
                    let agent_name = AgentId(name.clone().into());
                    self.external_agents.insert(
                        agent_name.clone(),
                        ExternalAgentEntry::new(
                            Box::new(LocalCustomAgent {
                                command: command.clone(),
                                agent_id: Arc::from(name.as_str()),
                                project_environment: project_environment.clone(),
                            }) as Box<dyn ExternalAgentServer>,
                            ExternalAgentSource::Custom,
                            None,
                            None,
                        ),
                    );
                }
                CustomAgentServerSettings::Registry { env, .. } => {
                    let Some(agent) = registry_agents_by_id.get(name) else {
                        if registry_store.is_some() {
                            log::debug!("Registry agent '{}' not found in ACP registry", name);
                        }
                        continue;
                    };

                    let agent_name = AgentId(name.clone().into());
                    match agent {
                        RegistryAgent::Binary(agent) => {
                            if !agent.supports_current_platform {
                                log::warn!(
                                    "Registry agent '{}' has no compatible binary for this platform",
                                    name
                                );
                                continue;
                            }

                            self.external_agents.insert(
                                agent_name.clone(),
                                ExternalAgentEntry::new(
                                    Box::new(LocalRegistryArchiveAgent {
                                        fs: fs.clone(),
                                        http_client: http_client.clone(),
                                        node_runtime: node_runtime.clone(),
                                        project_environment: project_environment.clone(),
                                        registry_id: Arc::from(name.as_str()),
                                        installation_dir: registry_archive_installation_dir(name),
                                        version: agent.metadata.version.clone(),
                                        targets: agent.targets.clone(),
                                        env: env.clone(),
                                        update_approvals: update_approvals.clone(),
                                        new_version_available_tx: NewVersionSlot::default(),
                                        loading_status_tx: None,
                                    })
                                        as Box<dyn ExternalAgentServer>,
                                    ExternalAgentSource::Registry,
                                    agent.metadata.icon_path.clone(),
                                    Some(agent.metadata.name.clone()),
                                ),
                            );
                        }
                        RegistryAgent::Npx(agent) => {
                            self.external_agents.insert(
                                agent_name.clone(),
                                ExternalAgentEntry::new(
                                    Box::new(LocalRegistryNpxAgent {
                                        fs: fs.clone(),
                                        http_client: http_client.clone(),
                                        node_runtime: node_runtime.clone(),
                                        project_environment: project_environment.clone(),
                                        registry_id: Arc::from(name.as_str()),
                                        version: agent.metadata.version.clone(),
                                        package: agent.package.clone(),
                                        args: agent.args.clone(),
                                        distribution_env: agent.env.clone(),
                                        settings_env: env.clone(),
                                        update_approvals: update_approvals.clone(),
                                        new_version_available_tx: NewVersionSlot::default(),
                                        loading_status_tx: None,
                                    })
                                        as Box<dyn ExternalAgentServer>,
                                    ExternalAgentSource::Registry,
                                    agent.metadata.icon_path.clone(),
                                    Some(agent.metadata.name.clone()),
                                ),
                            );
                        }
                    }
                }
            }
        }

        // For each rebuilt versioned agent, compare the version. If it
        // changed, notify the active connection to reconnect. Otherwise,
        // transfer the channel to the new entry so future updates can use it.
        for (name, entry) in &mut self.external_agents {
            let Some((old_version, new_version_available_tx, loading_status_tx)) =
                old_versioned_agents.remove(name)
            else {
                continue;
            };
            let Some(new_version) = entry.server.version() else {
                continue;
            };

            if new_version != &old_version {
                if let Some(mut tx) = new_version_available_tx {
                    tx.send(Some(new_version.to_string())).ok();
                }
            } else {
                if let Some(tx) = new_version_available_tx {
                    entry.server.set_new_version_available_tx(tx);
                }
                if let Some(tx) = loading_status_tx {
                    entry.server.set_loading_status_tx(tx);
                }
            }
        }

        *old_settings = Some(new_settings);

        if let Some((project_id, downstream_client)) = downstream_client {
            downstream_client
                .send(proto::ExternalAgentsUpdated {
                    project_id: *project_id,
                    names: self
                        .external_agents
                        .keys()
                        .map(|name| name.to_string())
                        .collect(),
                })
                .log_err();
        }
        cx.emit(AgentServersUpdated);
    }

    pub fn node_runtime(&self) -> Option<NodeRuntime> {
        match &self.state {
            AgentServerStoreState::Local { node_runtime, .. } => Some(node_runtime.clone()),
            _ => None,
        }
    }

    pub fn local(
        node_runtime: NodeRuntime,
        fs: Arc<dyn Fs>,
        project_environment: Entity<ProjectEnvironment>,
        http_client: Arc<dyn HttpClient>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.observe_global::<SettingsStore>(|this, cx| {
            this.agent_servers_settings_changed(cx);
        })];
        if let Some(registry_store) = AgentRegistryStore::try_global(cx) {
            subscriptions.push(cx.observe(&registry_store, |this, _, cx| {
                this.reregister_agents(cx);
            }));
        }
        let mut this = Self {
            state: AgentServerStoreState::Local {
                node_runtime,
                fs,
                project_environment,
                http_client,
                downstream_client: None,
                settings: None,
                _subscriptions: subscriptions,
            },
            external_agents: HashMap::default(),
            update_approvals: local_agent_update_approvals(cx),
        };
        this.agent_servers_settings_changed(cx);
        this
    }

    pub(crate) fn remote(
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
        worktree_store: Entity<WorktreeStore>,
    ) -> Self {
        Self {
            state: AgentServerStoreState::Remote {
                project_id,
                upstream_client,
                worktree_store,
            },
            external_agents: HashMap::default(),
            update_approvals: AgentUpdateApprovals::default(),
        }
    }

    pub fn collab() -> Self {
        Self {
            state: AgentServerStoreState::Collab,
            external_agents: HashMap::default(),
            update_approvals: AgentUpdateApprovals::default(),
        }
    }

    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) {
        match &mut self.state {
            AgentServerStoreState::Local {
                downstream_client, ..
            } => {
                *downstream_client = Some((project_id, client.clone()));
                // Send the current list of external agents downstream, but only after a delay,
                // to avoid having the message arrive before the downstream project's agent server store
                // sets up its handlers.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let names = this.update(cx, |this, _| {
                        this.external_agents()
                            .map(|name| name.to_string())
                            .collect()
                    })?;
                    client
                        .send(proto::ExternalAgentsUpdated { project_id, names })
                        .log_err();
                    anyhow::Ok(())
                })
                .detach();
            }
            AgentServerStoreState::Remote { .. } => {
                debug_panic!(
                    "external agents over collab not implemented, remote project should not be shared"
                );
            }
            AgentServerStoreState::Collab => {
                debug_panic!("external agents over collab not implemented, should not be shared");
            }
        }
    }

    pub fn get_external_agent(
        &mut self,
        name: &AgentId,
    ) -> Option<&mut (dyn ExternalAgentServer + 'static)> {
        self.external_agents
            .get_mut(name)
            .map(|entry| entry.server.as_mut())
    }

    pub fn no_browser(&self) -> bool {
        match &self.state {
            AgentServerStoreState::Local {
                downstream_client, ..
            } => downstream_client
                .as_ref()
                .is_some_and(|(_, client)| !client.has_wsl_interop()),
            _ => false,
        }
    }

    pub fn has_external_agents(&self) -> bool {
        !self.external_agents.is_empty()
    }

    pub fn external_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.external_agents.keys()
    }

    async fn handle_get_agent_server_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetAgentServerCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::AgentServerCommand> {
        let command = this
            .update(&mut cx, |this, cx| {
                let AgentServerStoreState::Local {
                    downstream_client, ..
                } = &this.state
                else {
                    debug_panic!("should not receive GetAgentServerCommand in a non-local project");
                    bail!("unexpected GetAgentServerCommand request in a non-local project");
                };
                let no_browser = this.no_browser();
                // ADR-F99: the client's user agreed to update the agent this
                // host installs, so this start installs the registry's
                // version.
                // The start below is on this host (`AgentLaunchTarget::Host`).
                if envelope.payload.approve_update == Some(true) {
                    this.update_approvals.approve(
                        &AgentId::new(envelope.payload.name.clone()),
                        AgentInstallPlace::Host,
                    );
                }
                let agent = this
                    .external_agents
                    .get_mut(&*envelope.payload.name)
                    .map(|entry| entry.server.as_mut())
                    .with_context(|| format!("agent `{}` not found", envelope.payload.name))?;
                let new_version_available_tx =
                    downstream_client
                        .clone()
                        .map(|(project_id, downstream_client)| {
                            let (new_version_available_tx, mut new_version_available_rx) =
                                watch::channel(None);
                            cx.spawn({
                                let name = envelope.payload.name.clone();
                                async move |_, _| {
                                    if let Some(version) =
                                        new_version_available_rx.recv().await.ok().flatten()
                                    {
                                        downstream_client.send(
                                            proto::NewExternalAgentVersionAvailable {
                                                project_id,
                                                name: name.clone(),
                                                version,
                                            },
                                        )?;
                                    }
                                    anyhow::Ok(())
                                }
                            })
                            .detach_and_log_err(cx);
                            new_version_available_tx
                        });
                let loading_status_tx =
                    downstream_client
                        .clone()
                        .map(|(project_id, downstream_client)| {
                            let (loading_status_tx, mut loading_status_rx) = watch::channel(None);
                            cx.spawn({
                                let name = envelope.payload.name.clone();
                                async move |_, _| {
                                    while let Ok(status) = loading_status_rx.recv().await {
                                        downstream_client.send(
                                            proto::ExternalAgentLoadingStatusUpdated {
                                                project_id,
                                                name: name.clone(),
                                                status,
                                            },
                                        )?;
                                    }
                                    anyhow::Ok(())
                                }
                            })
                            .detach_and_log_err(cx);
                            loading_status_tx
                        });
                let mut extra_env = HashMap::default();
                if no_browser {
                    extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
                }
                if let Some(new_version_available_tx) = new_version_available_tx {
                    agent.set_new_version_available_tx(new_version_available_tx);
                }
                if let Some(loading_status_tx) = loading_status_tx {
                    agent.set_loading_status_tx(loading_status_tx);
                }
                anyhow::Ok(agent.get_command(
                    vec![],
                    extra_env,
                    AgentLaunchTarget::Host,
                    &mut cx.to_async(),
                ))
            })?
            .await?;

        // ADR-F14: this handler only runs in `remote_server`, and the agent it
        // describes is spawned on this host, so the egress proxy to point it
        // at is this host's — the one `AcpConnection::stdio` deliberately
        // withholds from a remote agent because a loopback port there means a
        // different machine. Injected after the agent's own settings have had
        // their say, the same ordering as the local path, so a proxy spelled
        // out in `agent_servers.<name>.env` still wins.
        let env = host_route_environment(command.env);

        let (sandbox, sandbox_error) = match command.sandbox {
            Some(sandbox) if !envelope.payload.project_roots.is_empty() => {
                match Self::place_agent_sandbox_on_this_host(
                    &this,
                    sandbox,
                    &envelope.payload.project_roots,
                    &mut cx,
                )
                .await
                {
                    Ok(sandbox) => (sandbox, None),
                    // The client then refuses to start the agent and offers
                    // to start it outside the sandbox instead (ADR-F49
                    // decision 19); failing the whole request would take that
                    // choice away.
                    Err(error) => {
                        log::warn!("could not place an agent's sandbox on this host: {error:#}");
                        (
                            None,
                            Some(i18n::t!(
                                "Could not prepare the agent's settings folder on the remote host: {error}",
                                error = format!("{error:#}")
                            )),
                        )
                    }
                }
            }
            _ => (None, None),
        };

        Ok(proto::AgentServerCommand {
            path: command.path.to_string_lossy().into_owned(),
            args: command.args,
            env: env.into_iter().collect(),
            root_dir: envelope
                .payload
                .root_dir
                .unwrap_or_else(|| paths::home_dir().to_string_lossy().to_string()),
            login: None,
            sandbox,
            sandbox_error,
        })
    }

    /// ADR-F49: places a remote agent in the project that asked for it, on
    /// this host — the one it runs on — and describes it for the client,
    /// which starts it in this host's sandbox helper. Nothing is placed for a
    /// fake file system (tests), whose paths are not this host's.
    async fn place_agent_sandbox_on_this_host(
        this: &Entity<Self>,
        sandbox: AgentSandboxCommand,
        project_roots: &[String],
        cx: &mut AsyncApp,
    ) -> Result<Option<proto::AgentServerSandbox>> {
        // Only a Linux host's helper takes an agent, and only a Unix one can
        // make the folder safely.
        if !cfg!(unix) {
            return Ok(None);
        }
        let fs = this.read_with(cx, |this, _| match &this.state {
            AgentServerStoreState::Local { fs, .. } => Some(fs.clone()),
            AgentServerStoreState::Remote { .. } | AgentServerStoreState::Collab => None,
        });
        let Some(fs) = fs.filter(|fs| !fs.is_fake()) else {
            return Ok(None);
        };
        let canonical_roots = crate::agent_sandbox::canonical_project_key_roots(
            &fs,
            project_roots.iter().map(PathBuf::from).collect(),
        )
        .await;
        let placed = cx
            .background_spawn(async move {
                let home = crate::agent_sandbox::sandboxed_agents_home();
                sandbox.resolve_for_project(&home, &canonical_roots)
            })
            .await?;
        let AgentSandboxState::Resolved { config_dir, hosts } = placed.state else {
            bail!("the agent's sandbox was not placed in the project");
        };
        let (hosts, hosts_from_editor_allowlist) = match hosts {
            AgentSandboxHosts::Listed(hosts) => (hosts, false),
            AgentSandboxHosts::EditorAllowlist => (Vec::new(), true),
        };
        Ok(Some(proto::AgentServerSandbox {
            config_dir: config_dir.to_string_lossy().into_owned(),
            env: placed.env.into_iter().collect(),
            readable_paths: placed
                .readable_paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            hosts,
            hosts_from_editor_allowlist,
        }))
    }

    async fn handle_external_agents_updated(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ExternalAgentsUpdated>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let AgentServerStoreState::Remote {
                project_id,
                upstream_client,
                worktree_store,
            } = &this.state
            else {
                debug_panic!(
                    "handle_external_agents_updated should not be called for a non-remote project"
                );
                bail!("unexpected ExternalAgentsUpdated message")
            };

            let mut previous_entries = std::mem::take(&mut this.external_agents);
            let mut new_version_available_txs = HashMap::default();
            let mut loading_status_txs = HashMap::default();
            let mut metadata = HashMap::default();

            for (name, mut entry) in previous_entries.drain() {
                if let Some(tx) = entry.server.take_new_version_available_tx() {
                    new_version_available_txs.insert(name.clone(), tx);
                }
                if let Some(tx) = entry.server.take_loading_status_tx() {
                    loading_status_txs.insert(name.clone(), tx);
                }

                metadata.insert(name, (entry.icon, entry.display_name, entry.source));
            }

            this.external_agents = envelope
                .payload
                .names
                .into_iter()
                .map(|name| {
                    let agent_id = AgentId(name.into());
                    let (icon, display_name, source) = metadata
                        .remove(&agent_id)
                        .or_else(|| {
                            AgentRegistryStore::try_global(cx)
                                .and_then(|store| store.read(cx).agent(&agent_id))
                                .map(|s| {
                                    (
                                        s.icon_path().cloned(),
                                        Some(s.name().clone()),
                                        ExternalAgentSource::Registry,
                                    )
                                })
                        })
                        .unwrap_or((None, None, ExternalAgentSource::default()));
                    let agent = RemoteExternalAgentServer {
                        project_id: *project_id,
                        upstream_client: upstream_client.clone(),
                        worktree_store: worktree_store.clone(),
                        name: agent_id.clone(),
                        source,
                        update_approvals: this.update_approvals.clone(),
                        new_version_available_tx: new_version_available_txs.remove(&agent_id),
                        loading_status_tx: loading_status_txs.remove(&agent_id),
                    };
                    (
                        agent_id,
                        ExternalAgentEntry::new(
                            Box::new(agent) as Box<dyn ExternalAgentServer>,
                            source,
                            icon,
                            display_name,
                        ),
                    )
                })
                .collect();
            cx.emit(AgentServersUpdated);
            Ok(())
        })
    }

    async fn handle_loading_status_updated(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ExternalAgentLoadingStatusUpdated>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            if let Some(entry) = this.external_agents.get_mut(&*envelope.payload.name)
                && let Some(mut tx) = entry.server.take_loading_status_tx()
            {
                tx.send(envelope.payload.status).ok();
                entry.server.set_loading_status_tx(tx);
            }
        });
        Ok(())
    }

    async fn handle_new_version_available(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::NewExternalAgentVersionAvailable>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            if let Some(entry) = this.external_agents.get_mut(&*envelope.payload.name)
                && let Some(mut tx) = entry.server.take_new_version_available_tx()
            {
                tx.send(Some(envelope.payload.version)).ok();
                entry.server.set_new_version_available_tx(tx);
            }
        });
        Ok(())
    }
}

/// ADR-F49 決定 13: an agent's own environment, in the order that decides
/// what wins — what its distribution needs, then what the IDE adds, then the
/// agent's settings, which are the user's own word and override both.
///
/// Every route that starts a registry agent on this host (outside its
/// sandbox, and in the WSL sandbox, which builds its environment from nothing)
/// composes it here, so the order holds for all of them. ADR-F88: an API key
/// the card saved comes from the credential store in `extra_env`, so one the
/// user spells out in `agent_servers.<id>.env` still wins over it.
fn agent_environment(
    distribution_env: HashMap<String, String>,
    extra_env: HashMap<String, String>,
    settings_env: HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = distribution_env;
    env.extend(extra_env);
    env.extend(
        settings_env
            .into_iter()
            .filter(|(name, value)| !is_blank_stored_api_key(name, value)),
    );
    env
}

/// ADR-F88: the card and the migration take a blank key in the settings as no
/// key at all, so it must not shadow the stored one either: the card would say
/// the key is saved while the agent got an empty one.
fn is_blank_stored_api_key(name: &str, value: &str) -> bool {
    STORED_API_KEY_VARIABLES.contains(&name) && value.trim().is_empty()
}

/// ADR-F88: the names in `extra_env` that carry an API key the client read
/// from its credential store. Routes that apply `extra_env` over the agent's
/// own resolved environment (a custom agent's settings, a remote host's
/// command) let these only fill in what that environment lacks, so a key the
/// user spelled out there wins over the stored one, as on the registry route
/// ([`agent_environment`]).
pub const STORED_API_KEY_VARIABLES: &[&str] = &["ANTHROPIC_API_KEY"];

/// Lays `extra_env` over `env`, except that a [`STORED_API_KEY_VARIABLES`]
/// entry only fills in a name `env` does not have (or has blank).
fn apply_extra_env<S: std::hash::BuildHasher>(
    env: &mut std::collections::HashMap<String, String, S>,
    extra_env: HashMap<String, String>,
) {
    for (name, value) in extra_env {
        if STORED_API_KEY_VARIABLES.contains(&name.as_str()) {
            let current = env.entry(name.clone()).or_default();
            if is_blank_stored_api_key(&name, current) {
                *current = value;
            }
        } else {
            env.insert(name, value);
        }
    }
}

/// The environment of an agent this host is asked to describe, as it runs
/// outside its sandbox: what the agent's settings and distribution resolved,
/// plus this host's egress proxy.
///
/// ADR-F14: this only runs in `remote_server`, and the agent it describes is
/// spawned on this host, so the proxy to point it at is this host's — the one
/// `AcpConnection::stdio` deliberately withholds from a remote agent because a
/// loopback port there means a different machine. Injected after the agent's
/// own settings have had their say, the same ordering as the local path, so a
/// proxy spelled out in `agent_servers.<name>.env` still wins.
///
/// No configuration folder is named here (8c, the rest of ADR-F55): outside
/// the sandbox the agent can read the account's `~/.claude` anyway, so a
/// folder of the fork's would guard nothing and only make the user sign in
/// twice. A `CLAUDE_CONFIG_DIR` the user spelled out in the agent's settings is
/// already in `command_env` and stays. The sandbox's own per-project folder is
/// laid over this when the sandbox is placed.
fn host_route_environment(command_env: Option<HashMap<String, String>>) -> HashMap<String, String> {
    let mut env = command_env.unwrap_or_default();
    if http_client::child_process_proxy().is_some() {
        http_client::apply_child_process_proxy_env(&mut env);
    }
    env
}

/// ADR-F49 MEDIUM-5: what a host it connects to says it placed for the agent,
/// taken only as far as this client's own policy for the agent goes — its
/// folder is the one the profile names, the environment that points the agent
/// at it is this side's, and the hosts are within the profile's. Everything
/// else the host resolved (the agent's own settings and distribution) is its
/// to decide: it is the host the agent runs on.
fn accept_remote_agent_sandbox(
    profile: &SandboxedAgentProfile,
    sandbox: proto::AgentServerSandbox,
    extra_env: &HashMap<String, String>,
) -> Result<AgentSandboxCommand> {
    let config_dir = crate::agent_sandbox::accept_remote_config_dir(&sandbox.config_dir, profile)?;
    let hosts = crate::agent_sandbox::accept_remote_hosts(
        profile,
        if sandbox.hosts_from_editor_allowlist {
            AgentSandboxHosts::EditorAllowlist
        } else {
            AgentSandboxHosts::Listed(sandbox.hosts)
        },
    );
    let mut env: HashMap<String, String> = sandbox.env.into_iter().collect();
    // What the host resolved — the agent's settings and its distribution —
    // wins over what this side adds.
    for (name, value) in extra_env {
        env.entry(name.clone()).or_insert_with(|| value.clone());
    }
    // The names that confine the agent are this side's, spelled for the
    // host's file system.
    env.extend(profile.posix_environment(&sandbox.config_dir));
    Ok(AgentSandboxCommand {
        env,
        readable_paths: sandbox
            .readable_paths
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        state: AgentSandboxState::Resolved { config_dir, hosts },
        wsl: None,
    })
}

struct RemoteExternalAgentServer {
    project_id: u64,
    upstream_client: Entity<RemoteClient>,
    worktree_store: Entity<WorktreeStore>,
    name: AgentId,
    /// ADR-F49: where the agent came from, which with its id decides its
    /// profile — on this side, never on the host's word.
    source: ExternalAgentSource,
    update_approvals: AgentUpdateApprovals,
    new_version_available_tx: Option<watch::Sender<Option<String>>>,
    loading_status_tx: Option<watch::Sender<Option<String>>>,
}

impl RemoteExternalAgentServer {
    fn command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        _target: AgentLaunchTarget,
        claim: UpdateApprovalClaim,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let project_id = self.project_id;
        let name = self.name.to_string();
        // A remote host's store keeps the agreements for its installs as this
        // host's ([`AgentInstallPlace::Host`]).
        let approve_update = claim.take(&AgentInstallPlace::Host);
        let upstream_client = self.upstream_client.downgrade();
        let worktree_store = self.worktree_store.clone();
        // ADR-F49 MEDIUM-5: the agent's policy is this side's, from its id and
        // where it came from, whatever the host says.
        let profile = SandboxedAgentProfile::for_agent(
            self.name.as_ref(),
            matches!(self.source, ExternalAgentSource::Registry),
        );
        cx.spawn(async move |cx| {
            let project_roots = worktree_store.read_with(cx, |worktree_store, cx| {
                crate::agent_sandbox::project_key_roots(worktree_store, cx)
            });
            let root_dir = project_roots.first().map(|path| path.display().to_string());

            let mut response = upstream_client
                .update(cx, |upstream_client, _| {
                    upstream_client
                        .proto_client()
                        .request(proto::GetAgentServerCommand {
                            project_id,
                            name,
                            root_dir,
                            project_roots: project_roots
                                .iter()
                                .map(|path| path.to_string_lossy().into_owned())
                                .collect(),
                            approve_update: approve_update.then_some(true),
                        })
                })?
                .await?;
            let mut sandbox_error = response.sandbox_error.take();
            let sandbox = response.sandbox.take().and_then(|sandbox| {
                match accept_remote_agent_sandbox(&profile, sandbox, &extra_env) {
                    Ok(sandbox) => Some(sandbox),
                    // Refusing here rather than starting the agent under a
                    // policy this side did not decide (ADR-F49 MEDIUM-5); the
                    // client then offers to start it outside the sandbox.
                    Err(error) => {
                        log::warn!("the host's agent sandbox was refused: {error:#}");
                        sandbox_error.get_or_insert_with(|| {
                            i18n::t!(
                                "The agent sandbox the remote host prepared does not fit this editor's policy: {error}",
                                error = format!("{error:#}")
                            )
                        });
                        None
                    }
                }
            });
            let sandbox = sandbox.or_else(|| {
                sandbox_error.map(|reason| AgentSandboxCommand {
                    env: HashMap::default(),
                    readable_paths: Vec::new(),
                    state: AgentSandboxState::Unavailable { reason },
                    wsl: None,
                })
            });
            response.args.extend(extra_args);
            apply_extra_env(&mut response.env, extra_env);

            Ok(AgentServerCommand {
                path: response.path.into(),
                args: response.args,
                env: Some(response.env.into_iter().collect()),
                sandbox,
            })
        })
    }
}

impl ExternalAgentServer for RemoteExternalAgentServer {
    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.new_version_available_tx = Some(tx);
    }

    fn take_loading_status_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.loading_status_tx.take()
    }

    fn set_loading_status_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.loading_status_tx = Some(tx);
    }

    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::for_start(&self.update_approvals, self.name.clone());
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn get_command_without_update(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::none(self.name.clone());
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RegistryArchiveKind {
    Archive(AssetKind),
    /// The archive URL points directly at an executable, per the ACP registry
    /// schema: "URL to download archive (.zip, .tar.gz, .tgz, .tar.bz2, .tbz2,
    /// or raw binary)".
    RawBinary {
        file_name: String,
    },
}

fn registry_archive_kind_for_url(archive_url: &str) -> Result<RegistryArchiveKind> {
    const UNSUPPORTED_SUFFIXES: &[&str] = &[
        // Installer formats explicitly rejected by the registry schema.
        ".dmg",
        ".pkg",
        ".deb",
        ".rpm",
        ".msi",
        ".appimage",
        // Archive formats we cannot extract; treating them as raw binaries
        // would produce a broken install.
        ".tar.xz",
        ".txz",
        ".tar",
        ".gz",
        ".bz2",
        ".xz",
        ".7z",
    ];

    let archive_path = Url::parse(archive_url)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| archive_url.to_string());
    let lowercase_path = archive_path.to_lowercase();

    if lowercase_path.ends_with(".zip") {
        Ok(RegistryArchiveKind::Archive(AssetKind::Zip))
    } else if lowercase_path.ends_with(".tar.gz") || lowercase_path.ends_with(".tgz") {
        Ok(RegistryArchiveKind::Archive(AssetKind::TarGz))
    } else if lowercase_path.ends_with(".tar.bz2") || lowercase_path.ends_with(".tbz2") {
        Ok(RegistryArchiveKind::Archive(AssetKind::TarBz2))
    } else if let Some(suffix) = UNSUPPORTED_SUFFIXES
        .iter()
        .find(|suffix| lowercase_path.ends_with(*suffix))
    {
        bail!("unsupported archive type {suffix} in URL: {archive_url}");
    } else {
        let file_name = raw_binary_file_name(&archive_path)
            .with_context(|| format!("determining binary file name from URL: {archive_url}"))?;
        Ok(RegistryArchiveKind::RawBinary { file_name })
    }
}

fn raw_binary_file_name(archive_path: &str) -> Result<String> {
    let last_segment = archive_path
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .context("URL has no file name")?;
    let file_name = percent_decode_str(last_segment)
        .decode_utf8()
        .context("file name is not valid UTF-8")?
        .into_owned();
    anyhow::ensure!(
        !file_name.is_empty()
            && file_name != "."
            && file_name != ".."
            && !file_name.contains(['/', '\\'])
            && !file_name.contains('\0'),
        "invalid binary file name: {file_name}"
    );
    Ok(file_name)
}

struct GithubReleaseArchive {
    repo_name_with_owner: String,
    tag: String,
    asset_name: String,
}

fn github_release_archive_from_url(archive_url: &str) -> Option<GithubReleaseArchive> {
    fn decode_path_segment(segment: &str) -> Option<String> {
        percent_decode_str(segment)
            .decode_utf8()
            .ok()
            .map(|segment| segment.into_owned())
    }

    let url = Url::parse(archive_url).ok()?;
    if url.scheme() != "https" || url.host_str()? != "github.com" {
        return None;
    }

    let segments = url.path_segments()?.collect::<Vec<_>>();
    if segments.len() < 6 || segments[2] != "releases" || segments[3] != "download" {
        return None;
    }

    Some(GithubReleaseArchive {
        repo_name_with_owner: format!("{}/{}", segments[0], segments[1]),
        tag: decode_path_segment(segments[4])?,
        asset_name: segments[5..]
            .iter()
            .map(|segment| decode_path_segment(segment))
            .collect::<Option<Vec<_>>>()?
            .join("/"),
    })
}

pub(crate) fn sanitize_path_component(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => character,
            _ => '-',
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn versioned_archive_cache_dir(
    base_dir: &Path,
    version: Option<&str>,
    archive_url: &str,
    sha256: Option<&str>,
) -> PathBuf {
    base_dir.join(archive_version_component(
        version.unwrap_or_default(),
        archive_url,
        sha256,
    ))
}

/// The directory name one version of one registry archive is kept under: its
/// version, and what identifies the archive itself, so a version republished
/// under another URL or digest gets a directory of its own. Shared with the
/// WSL staging of the same archive (ADR-F49 第 2b 段), so the same rule holds
/// inside the distro.
fn archive_version_component(version: &str, archive_url: &str, sha256: Option<&str>) -> String {
    let sanitized_version = sanitize_path_component(version);

    let mut version_hasher = Sha256::new();
    version_hasher.update(version.as_bytes());
    let version_hash = format!("{:x}", version_hasher.finalize());

    let mut archive_hasher = Sha256::new();
    archive_hasher.update(archive_url.as_bytes());
    if let Some(sha256) = sha256 {
        archive_hasher.update(b"\0sha256:");
        archive_hasher.update(sha256.to_ascii_lowercase().as_bytes());
    }
    let archive_hash = format!("{:x}", archive_hasher.finalize());

    format!(
        "v_{sanitized_version}_{}_{}",
        &version_hash[..16],
        &archive_hash[..16],
    )
}

// The `v_` prefix here must stay in sync with `versioned_archive_cache_dir`,
// so we only ever remove directories that we created ourselves.
const VERSIONED_ARCHIVE_CACHE_DIR_PREFIX: &str = "v_";

async fn remove_stale_versioned_archive_cache_dirs(
    fs: Arc<dyn Fs>,
    base_dir: &Path,
    current_version_dir: &Path,
) -> Result<()> {
    let Some(current_dir_name) = current_version_dir.file_name() else {
        return Ok(());
    };

    let current_mtime = fs
        .metadata(current_version_dir)
        .await
        .with_context(|| format!("reading metadata for {current_version_dir:?}"))?
        .with_context(|| format!("missing metadata for {current_version_dir:?}"))?
        .mtime;

    let mut entries = fs
        .read_dir(base_dir)
        .await
        .with_context(|| format!("reading archive cache directory {base_dir:?}"))?;

    while let Some(entry) = entries.next().await {
        let entry = entry.with_context(|| format!("reading entry in {base_dir:?}"))?;
        let Some(entry_name) = entry.file_name() else {
            continue;
        };

        if entry_name == current_dir_name
            || !entry_name
                .to_string_lossy()
                .starts_with(VERSIONED_ARCHIVE_CACHE_DIR_PREFIX)
        {
            continue;
        }

        let Some(entry_metadata) = fs.metadata(&entry).await.log_err().flatten() else {
            continue;
        };
        if !entry_metadata.is_dir {
            continue;
        }
        // Only remove directories that predate the current version's directory.
        // This avoids racing with a concurrent extraction of a different version
        // that finished after we cached the current version's mtime.
        if !current_mtime.bad_is_greater_than(entry_metadata.mtime) {
            continue;
        }

        fs.remove_dir(
            &entry,
            RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            },
        )
        .await
        .with_context(|| format!("removing stale archive cache directory {entry:?}"))?;
    }

    Ok(())
}

/// Where the host keeps the downloaded builds of the registry agent `registry_id`.
pub(crate) fn registry_archive_installation_dir(registry_id: &str) -> PathBuf {
    paths::external_agents_dir()
        .join("registry")
        .join(sanitize_path_component(registry_id))
}

/// The versions of the registry agent `registry_id` cached on this host, most
/// recently written first (F2-5 follow-up: the registry page's "update
/// available" badge). Empty when nothing is cached, which includes an agent
/// that has only ever run inside the WSL sandbox (its build is staged in the
/// distro, not here) and every `npx` agent (npm resolves a version range, so
/// there is no single installed version to read).
///
/// More than one version can be cached: the sweep of older versions only runs
/// on the next launch, and fails on files a still-running older build holds
/// open, which also leaves that older directory with the newest mtime. So ask
/// whether the registry's version is among these, rather than trusting the
/// first one to be what launches. Versions come back as the cache directory
/// spells them; compare them with [`registry_archive_version_matches`], never
/// with `==`.
pub async fn installed_registry_archive_versions(fs: &dyn Fs, registry_id: &str) -> Vec<String> {
    versioned_archive_versions_newest_first(fs, &registry_archive_installation_dir(registry_id))
        .await
}

/// Whether `installed_version` (from [`installed_registry_archive_versions`]) is
/// the registry's `registry_version`.
pub fn registry_archive_version_matches(installed_version: &str, registry_version: &str) -> bool {
    installed_version == sanitize_path_component(registry_version)
}

async fn versioned_archive_versions_newest_first(fs: &dyn Fs, base_dir: &Path) -> Vec<String> {
    if !fs.is_dir(base_dir).await {
        return Vec::new();
    }
    let Some(mut entries) = fs
        .read_dir(base_dir)
        .await
        .with_context(|| format!("reading archive cache directory {base_dir:?}"))
        .log_err()
    else {
        return Vec::new();
    };

    let mut versions: Vec<(fs::MTime, String)> = Vec::new();
    while let Some(entry) = entries.next().await {
        let Some(entry) = entry.log_err() else {
            continue;
        };
        let Some(version) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(versioned_archive_dir_version)
            .map(str::to_string)
        else {
            continue;
        };
        let Some(metadata) = fs.metadata(&entry).await.log_err().flatten() else {
            continue;
        };
        if !metadata.is_dir {
            continue;
        }
        let position = versions
            .iter()
            .position(|(mtime, _)| metadata.mtime.bad_is_greater_than(*mtime))
            .unwrap_or(versions.len());
        versions.insert(position, (metadata.mtime, version));
    }
    versions.into_iter().map(|(_, version)| version).collect()
}

/// The version part of a directory name made by [`archive_version_component`].
fn versioned_archive_dir_version(directory_name: &str) -> Option<&str> {
    let rest = directory_name.strip_prefix(VERSIONED_ARCHIVE_CACHE_DIR_PREFIX)?;
    let mut parts = rest.rsplitn(3, '_');
    let archive_hash = parts.next()?;
    let version_hash = parts.next()?;
    let version = parts.next()?;
    let is_hash = |part: &str| part.len() == 16 && part.chars().all(|c| c.is_ascii_hexdigit());
    (is_hash(archive_hash) && is_hash(version_hash) && !version.is_empty()).then_some(version)
}

/// The SHA-256 a registry archive is checked against: the one the registry
/// names, or — for a GitHub release asset — the digest GitHub reports for it.
/// `None` when neither is available, which leaves HTTPS as the only assurance
/// (ADR-F49 決定 25).
async fn registry_archive_sha256(
    http_client: &Arc<dyn HttpClient>,
    archive_url: &str,
    provided: Option<String>,
) -> Option<String> {
    if provided.is_some() {
        return provided;
    }
    let github_archive = github_release_archive_from_url(archive_url)?;
    let release = ::http_client::github::get_release_by_tag_name(
        &github_archive.repo_name_with_owner,
        &github_archive.tag,
        http_client.clone(),
    )
    .await
    .ok()?;
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == github_archive.asset_name)?;
    asset.digest.as_ref().map(|digest| {
        digest
            .strip_prefix("sha256:")
            .map(ToString::to_string)
            .unwrap_or_else(|| digest.clone())
    })
}

/// ADR-F99: one build of a registry archive agent on this host, as a start
/// launches it.
#[derive(Clone, Debug, PartialEq)]
struct HostArchiveLaunch {
    archive_url: String,
    sha256: Option<String>,
    version_dir: PathBuf,
    cmd: String,
    args: Vec<String>,
    env: HashMap<String, String>,
}

impl HostArchiveLaunch {
    fn to_record(&self) -> InstalledAgentLaunch {
        InstalledAgentLaunch::Archive {
            archive_url: self.archive_url.clone(),
            sha256: self.sha256.clone(),
            directory: self.version_dir.to_string_lossy().into_owned(),
            cmd: self.cmd.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
        }
    }
}

/// The record at `record_path` and the build it describes, while its files
/// are still there to launch.
async fn usable_host_archive_record(
    fs: &dyn Fs,
    record_path: &Path,
) -> Option<(InstalledAgentRecord, HostArchiveLaunch)> {
    let record = read_installed_record(fs, record_path).await?;
    let InstalledAgentLaunch::Archive {
        archive_url,
        sha256,
        directory,
        cmd,
        args,
        env,
    } = record.launch.clone()
    else {
        return None;
    };
    let version_dir = PathBuf::from(directory);
    // The record is a file anyone who can write there can rewrite: it only
    // ever names one of the builds this installation directory holds, as the
    // registry's own description would.
    let is_a_build_here = version_dir.parent() == record_path.parent()
        && version_dir.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .starts_with(VERSIONED_ARCHIVE_CACHE_DIR_PREFIX)
        });
    if !is_a_build_here || !fs.is_dir(&version_dir).await {
        return None;
    }
    if cmd != "node" {
        let relative = cmd.strip_prefix("./").or_else(|| cmd.strip_prefix(".\\"))?;
        if cmd.contains("..") || !fs.is_file(&version_dir.join(relative)).await {
            return None;
        }
    }
    Some((
        record,
        HostArchiveLaunch {
            archive_url,
            sha256,
            version_dir,
            cmd,
            args,
            env,
        },
    ))
}

/// Installs the registry's `version` of an archive agent on this host (a
/// build already unpacked for it is kept), as every start did before
/// ADR-F99.
async fn install_host_registry_archive(
    fs: &dyn Fs,
    http_client: &Arc<dyn HttpClient>,
    installation_dir: &Path,
    version: &str,
    targets: &HashMap<String, RegistryTargetConfig>,
    mut loading_status_tx: Option<watch::Sender<Option<String>>>,
) -> Result<HostArchiveLaunch> {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        anyhow::bail!("unsupported OS");
    };

    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        anyhow::bail!("unsupported architecture");
    };

    let platform_key = format!("{}-{}", os, arch);
    let target_config = targets.get(&platform_key).with_context(|| {
        format!(
            "no target specified for platform '{}'. Available platforms: {}",
            platform_key,
            targets
                .keys()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let archive_url = &target_config.archive;
    let version_dir = versioned_archive_cache_dir(
        installation_dir,
        Some(version),
        archive_url,
        target_config.sha256.as_deref(),
    );

    let mut sha256 = target_config.sha256.clone();
    if !fs.is_dir(&version_dir).await {
        if let Some(tx) = loading_status_tx.as_mut() {
            tx.send(Some(format!("Installing {version}…"))).ok();
        }

        sha256 =
            registry_archive_sha256(http_client, archive_url, target_config.sha256.clone()).await;

        match registry_archive_kind_for_url(archive_url)? {
            RegistryArchiveKind::Archive(asset_kind) => {
                ::http_client::github_download::download_server_binary(
                    &**http_client,
                    archive_url,
                    sha256.as_deref(),
                    &version_dir,
                    asset_kind,
                )
                .await?;
            }
            RegistryArchiveKind::RawBinary { file_name } => {
                ::http_client::github_download::download_server_raw_binary(
                    &**http_client,
                    archive_url,
                    sha256.as_deref(),
                    &version_dir,
                    &file_name,
                )
                .await?;
            }
        }
    }

    Ok(HostArchiveLaunch {
        archive_url: archive_url.clone(),
        sha256,
        version_dir,
        cmd: target_config.cmd.clone(),
        args: target_config.args.clone(),
        env: target_config.env.clone(),
    })
}

struct LocalRegistryArchiveAgent {
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    node_runtime: NodeRuntime,
    project_environment: Entity<ProjectEnvironment>,
    registry_id: Arc<str>,
    installation_dir: PathBuf,
    version: SharedString,
    targets: HashMap<String, RegistryTargetConfig>,
    env: HashMap<String, String>,
    update_approvals: AgentUpdateApprovals,
    new_version_available_tx: NewVersionSlot,
    loading_status_tx: Option<watch::Sender<Option<String>>>,
}

impl LocalRegistryArchiveAgent {
    fn command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        claim: UpdateApprovalClaim,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let fs = self.fs.clone();
        let http_client = self.http_client.clone();
        let node_runtime = self.node_runtime.clone();
        let project_environment = self.project_environment.downgrade();
        let registry_id = self.registry_id.clone();
        let installation_dir = self.installation_dir.clone();
        let targets = self.targets.clone();
        let settings_env = self.env.clone();
        let version = self.version.clone();
        // ADR-F99: an update the user agreed to for the place this start
        // installs in is installed by it.
        let place = AgentInstallPlace::for_launch_target(&target);
        let update_approved = claim.take(&place);
        let new_version_available = self.new_version_available_tx.clone();
        // Only the WSL path reports progress step by step; the host path uses
        // it once, below.
        #[cfg_attr(not(target_os = "windows"), expect(unused_mut))]
        let mut loading_status_tx = self.loading_status_tx.take();

        cx.spawn(async move |cx| {
            // ADR-F49 第 2b 段: on Windows the agent runs inside the project's
            // WSL distro, so it is the entry's **Linux** build that is
            // obtained and staged, never this machine's Windows one.
            #[cfg_attr(not(target_os = "windows"), expect(unused_mut))]
            let mut sandbox_failure: Option<String> = None;
            #[cfg(target_os = "windows")]
            if let AgentLaunchTarget::Sandbox { wsl_distro } = &target {
                match registry_archive_command_in_wsl(
                    wsl_distro.clone(),
                    cx.background_executor().clone(),
                    http_client.clone(),
                    fs.clone(),
                    &registry_id,
                    version.as_ref(),
                    &targets,
                    extra_env.clone(),
                    settings_env.clone(),
                    extra_args.clone(),
                    update_approved,
                    &new_version_available,
                    &mut loading_status_tx,
                )
                .await
                {
                    Ok(command) => return Ok(command),
                    // Not fatal: the client shows the reason and offers to
                    // start the agent outside the sandbox once (決定 19).
                    Err(error) => {
                        log::warn!(
                            "could not resolve the agent `{registry_id}` \
                             for the WSL sandbox: {error:#}"
                        );
                        sandbox_failure = Some(format!("{error:#}"));
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            let _ = &target;

            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();

            let dir = installation_dir;
            fs.create_dir(&dir).await?;

            // ADR-F99: what is installed is launched as it is, without asking
            // the network anything, unless the user agreed to the registry's
            // newer version; a newer one is only reported. A start for the
            // sandbox that ends up here (the WSL distro could not take it)
            // installs on this host, so it is this host's agreement it takes.
            let update_approved = if place == AgentInstallPlace::Host {
                update_approved
            } else {
                claim.take(&AgentInstallPlace::Host)
            };
            let record_path = dir.join(INSTALLED_RECORD_FILE_NAME);
            let recorded = usable_host_archive_record(fs.as_ref(), &record_path).await;
            let decision = launch_decision(
                recorded
                    .as_ref()
                    .map(|(record, _)| record.registry_version.as_str()),
                version.as_ref(),
                update_approved,
            );
            let launch = match (decision, recorded) {
                (LaunchDecision::Recorded { newer }, Some((record, launch))) => {
                    if let Some(newer) = newer {
                        report_new_version(&new_version_available, newer);
                    }
                    record_start(fs.as_ref(), &record_path, record).await;
                    launch
                }
                _ => {
                    let launch = install_host_registry_archive(
                        fs.as_ref(),
                        &http_client,
                        &dir,
                        version.as_ref(),
                        &targets,
                        loading_status_tx,
                    )
                    .await?;
                    let record =
                        InstalledAgentRecord::started_now(version.to_string(), launch.to_record());
                    write_installed_record(fs.as_ref(), &record_path, &record)
                        .await
                        .log_err();
                    launch
                }
            };
            let HostArchiveLaunch {
                version_dir,
                cmd,
                args: launch_args,
                env: distribution_env,
                ..
            } = launch;

            // ADR-F49: the sandbox takes the agent's own environment only, not
            // the host's it is layered over here.
            let agent_env = agent_environment(distribution_env, extra_env, settings_env);
            env.extend(agent_env.clone());

            let cmd = &cmd;

            let cmd_path = if cmd == "node" {
                node_runtime.binary_path().await?
            } else {
                if cmd.contains("..") {
                    anyhow::bail!("command path cannot contain '..': {}", cmd);
                }

                if cmd.starts_with("./") || cmd.starts_with(".\\") {
                    let cmd_path = version_dir.join(&cmd[2..]);
                    anyhow::ensure!(
                        fs.is_file(&cmd_path).await,
                        "Missing command {} after extraction",
                        cmd_path.to_string_lossy()
                    );
                    cmd_path
                } else {
                    anyhow::bail!("command must be relative (start with './'): {}", cmd);
                }
            };

            cx.background_spawn({
                let fs = fs.clone();
                let dir = dir.clone();
                let version_dir = version_dir.clone();
                async move {
                    remove_stale_versioned_archive_cache_dirs(fs, &dir, &version_dir)
                        .await
                        .log_err();
                }
            })
            .detach();

            let mut args = launch_args;
            args.extend(extra_args);

            let mut runs_from = vec![dir.clone()];
            let managed_node = paths::data_dir().join("node");
            if cmd_path.starts_with(&managed_node) {
                runs_from.push(managed_node);
            }
            let command = AgentServerCommand {
                sandbox: Some(sandbox_placement_failed(
                    AgentSandboxCommand::new(
                        SandboxedAgentProfile::for_agent(&registry_id, true),
                        agent_env,
                        readable_paths_in_data_dir(paths::data_dir(), &runs_from),
                    ),
                    sandbox_failure,
                )),
                path: cmd_path,
                args,
                env: Some(env),
            };

            Ok(command)
        })
    }
}

impl ExternalAgentServer for LocalRegistryArchiveAgent {
    fn version(&self) -> Option<&SharedString> {
        Some(&self.version)
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.lock().take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        *self.new_version_available_tx.lock() = Some(tx);
    }

    fn take_loading_status_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.loading_status_tx.take()
    }

    fn set_loading_status_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.loading_status_tx = Some(tx);
    }

    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::for_start(
            &self.update_approvals,
            AgentId::new(self.registry_id.to_string()),
        );
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn get_command_without_update(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::none(AgentId::new(self.registry_id.to_string()));
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

struct LocalRegistryNpxAgent {
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    node_runtime: NodeRuntime,
    project_environment: Entity<ProjectEnvironment>,
    registry_id: Arc<str>,
    version: SharedString,
    package: SharedString,
    args: Vec<String>,
    distribution_env: HashMap<String, String>,
    settings_env: HashMap<String, String>,
    update_approvals: AgentUpdateApprovals,
    new_version_available_tx: NewVersionSlot,
    loading_status_tx: Option<watch::Sender<Option<String>>>,
}

impl LocalRegistryNpxAgent {
    fn command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        claim: UpdateApprovalClaim,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let fs = self.fs.clone();
        // Obtaining the Linux Node.js and saying how far it has got are the
        // WSL path's; the host path installs with the Node.js it already has.
        #[cfg_attr(not(target_os = "windows"), expect(unused_variables))]
        let http_client = self.http_client.clone();
        let node_runtime = self.node_runtime.clone();
        let project_environment = self.project_environment.downgrade();
        let registry_id = self.registry_id.clone();
        let package = self.package.clone();
        let args = self.args.clone();
        let distribution_env = self.distribution_env.clone();
        let settings_env = self.settings_env.clone();
        let version = self.version.clone();
        // ADR-F99: an update the user agreed to for the place this start
        // installs in is installed by it.
        let place = AgentInstallPlace::for_launch_target(&target);
        let update_approved = claim.take(&place);
        let new_version_available = self.new_version_available_tx.clone();
        #[cfg_attr(not(target_os = "windows"), expect(unused_mut, unused_variables))]
        let mut loading_status_tx = self.loading_status_tx.take();

        cx.spawn(async move |cx| {
            // ADR-F49 第 2b 段: on Windows the sandbox is the project's WSL
            // distro, where the agent is a Linux command run from a Linux
            // Node.js. What this host would install for itself is of no use
            // there (the package's native parts are chosen per platform by its
            // install scripts), so the whole runtime is resolved inside the
            // distro instead.
            #[cfg_attr(not(target_os = "windows"), expect(unused_mut))]
            let mut sandbox_failure: Option<String> = None;
            #[cfg(target_os = "windows")]
            if let AgentLaunchTarget::Sandbox { wsl_distro } = &target {
                let editor_network_allowlist =
                    cx.update(|cx| wsl_runtime::editor_network_allowlist(cx));
                match npx_agent_command_in_wsl(
                    wsl_distro.clone(),
                    cx.background_executor().clone(),
                    http_client.clone(),
                    fs.clone(),
                    &registry_id,
                    version.as_ref(),
                    &package,
                    args.clone(),
                    extra_args.clone(),
                    distribution_env.clone(),
                    extra_env.clone(),
                    settings_env.clone(),
                    editor_network_allowlist,
                    update_approved,
                    &new_version_available,
                    &mut loading_status_tx,
                )
                .await
                {
                    Ok(command) => return Ok(command),
                    // Not fatal: the client shows the reason and offers to
                    // start the agent outside the sandbox once (決定 19).
                    Err(error) => {
                        log::warn!(
                            "could not resolve the agent `{registry_id}` \
                             for the WSL sandbox: {error:#}"
                        );
                        sandbox_failure = Some(format!("{error:#}"));
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            let _ = &target;

            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();

            // A start for the sandbox that ends up here (the WSL distro could
            // not take it) installs on this host, so it is this host's
            // agreement it takes.
            let update_approved = if place == AgentInstallPlace::Host {
                update_approved
            } else {
                claim.take(&AgentInstallPlace::Host)
            };
            let install_dir = host_npx_install_dir(&registry_id);
            let (launch, newer) = resolve_host_npx_install(
                fs.as_ref(),
                &install_dir,
                version.as_ref(),
                &package,
                &args,
                &distribution_env,
                update_approved,
                async |package_spec: String, package_name: String| {
                    node_runtime
                        .run_npm_subcommand_through_egress_proxy(
                            Some(&install_dir),
                            "install",
                            &[package_spec.as_str(), "--save-exact"],
                        )
                        .await?;
                    node_runtime::read_package_executable(
                        install_dir.join("node_modules"),
                        &package_name,
                    )
                    .await
                },
            )
            .await?;
            if let Some(newer) = newer {
                report_new_version(&new_version_available, newer);
            }
            let HostNpxLaunch {
                executable,
                args,
                env: distribution_env,
            } = launch;

            let node_binary = node_runtime.binary_path().await?;
            // ADR-F49: in the sandbox the agent reaches the network through the
            // sandbox's own proxy, so the environment it takes there has none
            // of the child-process proxy's, and none of the host's.
            let own_env = agent_environment(distribution_env, extra_env, settings_env);
            let mut agent_env: HashMap<String, String> =
                node_runtime::npm_command_env(&node_binary, node_runtime::NpmProxyRouting::Direct)
                    .into_iter()
                    .collect();
            agent_env.extend(own_env.clone());
            // The agent process itself runs inside the egress gate (ADR-F14), so
            // its environment carries the child-process proxy along with PATH.
            env.extend(node_runtime::npm_command_env(
                &node_binary,
                node_runtime::NpmProxyRouting::ThroughEgressProxy,
            ));
            env.extend(own_env);

            let mut command_args = vec![executable.to_string_lossy().into_owned()];
            command_args.extend(args);
            command_args.extend(extra_args);

            let mut runs_from = vec![install_dir.clone()];
            let managed_node = paths::data_dir().join("node");
            if node_binary.starts_with(&managed_node) {
                runs_from.push(managed_node);
            }
            let command = AgentServerCommand {
                sandbox: Some(sandbox_placement_failed(
                    AgentSandboxCommand::new(
                        SandboxedAgentProfile::for_agent(&registry_id, true),
                        agent_env,
                        readable_paths_in_data_dir(paths::data_dir(), &runs_from),
                    ),
                    sandbox_failure,
                )),
                path: node_binary,
                args: command_args,
                env: Some(env),
            };

            Ok(command)
        })
    }
}

impl ExternalAgentServer for LocalRegistryNpxAgent {
    fn version(&self) -> Option<&SharedString> {
        Some(&self.version)
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.lock().take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        *self.new_version_available_tx.lock() = Some(tx);
    }

    fn take_loading_status_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.loading_status_tx.take()
    }

    fn set_loading_status_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.loading_status_tx = Some(tx);
    }

    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::for_start(
            &self.update_approvals,
            AgentId::new(self.registry_id.to_string()),
        );
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn get_command_without_update(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let claim = UpdateApprovalClaim::none(AgentId::new(self.registry_id.to_string()));
        self.command(extra_args, extra_env, target, claim, cx)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// ADR-F99: the npm package of a registry agent as this host launches it.
#[derive(Clone, Debug, PartialEq)]
struct HostNpxLaunch {
    executable: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
}

/// ADR-F99: whether a recorded executable is one `npm install` put in
/// `install_dir`. The record is a file anyone who can write there can
/// rewrite, so it never names a program from anywhere else.
fn is_installed_npm_file(install_dir: &Path, executable: &Path) -> bool {
    // Lexically, because a package's `bin` may climb out of its own directory
    // (into another package of the same install), but not out of the install.
    let mut normalized = PathBuf::new();
    for component in executable.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    return false;
                }
            }
            component => normalized.push(component),
        }
    }
    normalized.starts_with(install_dir.join("node_modules"))
}

/// ADR-F99: what a start of an npx agent launches on this host, and the
/// registry's version when it is newer than that and the user has not agreed
/// to it yet. `npm install` (`install`, given the bounded package
/// specification and the package's name, answering with its executable) only
/// runs when there is no usable record or the update was agreed to: every
/// other start asks the network nothing.
#[allow(
    clippy::too_many_arguments,
    reason = "one registry entry's npm distribution, and how to install it"
)]
async fn resolve_host_npx_install(
    fs: &dyn Fs,
    install_dir: &Path,
    registry_version: &str,
    package: &str,
    args: &[String],
    distribution_env: &HashMap<String, String>,
    update_approved: bool,
    install: impl AsyncFnOnce(String, String) -> Result<PathBuf>,
) -> Result<(HostNpxLaunch, Option<String>)> {
    let record_path = install_dir.join(INSTALLED_RECORD_FILE_NAME);
    let recorded = match read_installed_record(fs, &record_path).await {
        Some(record) => match record.launch.clone() {
            InstalledAgentLaunch::Npx {
                executable,
                args,
                env,
                ..
            } if is_installed_npm_file(install_dir, Path::new(&executable))
                && fs.is_file(Path::new(&executable)).await =>
            {
                Some((
                    record,
                    HostNpxLaunch {
                        executable: PathBuf::from(executable),
                        args,
                        env,
                    },
                ))
            }
            _ => None,
        },
        None => None,
    };
    let decision = launch_decision(
        recorded
            .as_ref()
            .map(|(record, _)| record.registry_version.as_str()),
        registry_version,
        update_approved,
    );
    if let (LaunchDecision::Recorded { newer }, Some((record, launch))) = (decision, recorded) {
        record_start(fs, &record_path, record).await;
        return Ok((launch, newer));
    }

    fs.create_dir(install_dir).await?;
    let (package_name, package_spec) = bounded_npm_package_spec(package);
    let executable = install(package_spec, package_name.to_string()).await?;
    let launch = HostNpxLaunch {
        executable,
        args: args.to_vec(),
        env: distribution_env.clone(),
    };
    let record = InstalledAgentRecord::started_now(
        registry_version.to_string(),
        InstalledAgentLaunch::Npx {
            package: package.to_string(),
            executable: launch.executable.to_string_lossy().into_owned(),
            args: launch.args.clone(),
            env: launch.env.clone(),
        },
    );
    write_installed_record(fs, &record_path, &record)
        .await
        .log_err();
    Ok((launch, None))
}

/// People are using min-release-age more frequently. Which means a fresh registry will likely have
/// new package versions than the user can install.
/// We set the version to now be a ceiling and not an exact pin instead. This allows npm to resolve
/// the latest version it can find that satisfies the constraint. npm seems to check regularly enough
/// that new versions are available. This does have a few downsides:
/// - The user might have an older cached version of the package that satisfies the constraint, until
///   npm checks for updates again.
/// - The registry args/env may not be valid for the resolved version.
///
/// This is a best-effort attempt to install a version that works without overriding the user's
/// security settings, as the args don't change often. The registry will need to support this better
/// at some point, but until then, this is a best-effort workaround that hopefully solves the issue
/// for most users.
///
/// We use npm's hyphen-range syntax (`0.0.0 - <version>`, equivalent to `<=<version>`) instead of
/// the more compact `<=<version>` form because on Windows, `npm` is `npm.cmd` (a batch file run by
/// cmd.exe), and the quotes our shell builder emits are PowerShell string-literal syntax that PS
/// strips during parsing. PS only re-adds CRT-style transport quotes around native command args
/// containing whitespace, so `package@<=0.25.3` reaches cmd.exe bare and the unquoted `<` is
/// interpreted as input redirection. See zed-industries/zed#55921.
fn bounded_npm_package_spec(package_spec: &str) -> (&str, String) {
    let Some((package_name, version)) = package_spec.rsplit_once('@') else {
        return (package_spec, package_spec.to_string());
    };
    if package_name.is_empty() {
        return (package_spec, package_spec.to_string());
    }
    if Version::parse(version).is_err() {
        return (package_name, package_spec.to_string());
    }

    (package_name, format!("{package_name}@0.0.0 - {version}"))
}

struct LocalCustomAgent {
    project_environment: Entity<ProjectEnvironment>,
    agent_id: Arc<str>,
    command: AgentServerCommand,
}

impl ExternalAgentServer for LocalCustomAgent {
    fn get_command(
        &mut self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        target: AgentLaunchTarget,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let mut command = self.command.clone();
        let project_environment = self.project_environment.downgrade();
        let profile = SandboxedAgentProfile::for_agent(&self.agent_id, false);
        cx.spawn(async move |cx| {
            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();
            // ADR-F49: the sandbox takes the agent's own environment only.
            let mut agent_env = command.env.take().unwrap_or_default();
            apply_extra_env(&mut agent_env, extra_env);
            env.extend(agent_env.clone());

            // ADR-F49 第 2b 段: on Windows the sandbox is the project's WSL
            // distro, which can only run a Linux program. A command spelled as
            // a Windows path is reported as one the sandbox cannot take,
            // rather than started outside it without saying so (決定 19).
            #[cfg(target_os = "windows")]
            if let AgentLaunchTarget::Sandbox { wsl_distro } = target {
                let mut sandbox = AgentSandboxCommand::new(profile, agent_env, Vec::new());
                let path = command.path.to_string_lossy().into_owned();
                if path.starts_with('/') && !path.starts_with("//") {
                    sandbox.wsl = Some(WslAgentRuntime { distro: wsl_distro });
                    // Everything a Linux command needs is in `sandbox.env`,
                    // which the WSL helper sets inside the sandbox; this
                    // machine's environment means nothing there.
                    command.env = None;
                } else {
                    sandbox.state = AgentSandboxState::Unavailable {
                        reason: i18n::t!(
                            "The Windows program `{path}` cannot run inside the WSL sandbox. Set \
                             the absolute path of a Linux program (such as `/usr/local/bin/...`) \
                             in the agent's settings, or start it outside the sandbox."
                        ),
                    };
                    command.env = Some(env);
                }
                command.args.extend(extra_args);
                command.sandbox = Some(sandbox);
                return Ok(command);
            }
            #[cfg(not(target_os = "windows"))]
            let _ = &target;

            command.env = Some(env);
            command.args.extend(extra_args);
            command.sandbox = Some(AgentSandboxCommand::new(profile, agent_env, Vec::new()));
            Ok(command)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Default, Clone, JsonSchema, Debug, PartialEq, RegisterSetting)]
pub struct AllAgentServersSettings(pub HashMap<String, CustomAgentServerSettings>);

impl std::ops::Deref for AllAgentServersSettings {
    type Target = HashMap<String, CustomAgentServerSettings>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for AllAgentServersSettings {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AllAgentServersSettings {
    pub fn has_registry_agents(&self) -> bool {
        self.values()
            .any(|s| matches!(s, CustomAgentServerSettings::Registry { .. }))
    }
}

#[derive(Clone, JsonSchema, Debug, PartialEq)]
pub enum CustomAgentServerSettings {
    Custom {
        command: AgentServerCommand,
        /// The default mode to use for this agent.
        ///
        /// Note: Not only all agents support modes.
        ///
        /// Default: None
        default_mode: Option<String>,
        /// Default values for session config options.
        ///
        /// This is a map from config option ID to the default value for that option.
        ///
        /// Default: {}
        default_config_options: HashMap<String, AgentConfigOptionValue>,
        /// Favorited values for session config options.
        ///
        /// This is a map from config option ID to a list of favorited value IDs.
        ///
        /// Default: {}
        favorite_config_option_values: HashMap<String, Vec<String>>,
        /// ADR-F55: whether a subscription sign-in may be offered for this
        /// agent. `None` is the default, which is that it may.
        allow_subscription_sign_in: Option<bool>,
        /// ADR-F49 decision 19: whether this agent always starts outside its
        /// sandbox. Only the user's own settings file is read for it
        /// ([`crate::agent_sandbox::runs_outside_sandbox_by_user_choice`]).
        run_outside_sandbox: Option<bool>,
    },
    Registry {
        /// Additional environment variables to pass to the agent.
        ///
        /// Default: {}
        env: HashMap<String, String>,
        /// The default mode to use for this agent.
        ///
        /// Note: Not only all agents support modes.
        ///
        /// Default: None
        default_mode: Option<String>,
        /// Default values for session config options.
        ///
        /// This is a map from config option ID to the default value for that option.
        ///
        /// Default: {}
        default_config_options: HashMap<String, AgentConfigOptionValue>,
        /// Favorited values for session config options.
        ///
        /// This is a map from config option ID to a list of favorited value IDs.
        ///
        /// Default: {}
        favorite_config_option_values: HashMap<String, Vec<String>>,
        /// ADR-F55: whether a subscription sign-in may be offered for this
        /// agent. `None` is the default, which is that it may.
        allow_subscription_sign_in: Option<bool>,
        /// ADR-F49 decision 19: whether this agent always starts outside its
        /// sandbox. Only the user's own settings file is read for it
        /// ([`crate::agent_sandbox::runs_outside_sandbox_by_user_choice`]).
        run_outside_sandbox: Option<bool>,
    },
}

impl CustomAgentServerSettings {
    pub fn command(&self) -> Option<&AgentServerCommand> {
        match self {
            CustomAgentServerSettings::Custom { command, .. } => Some(command),
            CustomAgentServerSettings::Registry { .. } => None,
        }
    }

    pub fn default_mode(&self) -> Option<&str> {
        match self {
            CustomAgentServerSettings::Custom { default_mode, .. }
            | CustomAgentServerSettings::Registry { default_mode, .. } => default_mode.as_deref(),
        }
    }

    pub fn default_config_option(&self, config_id: &str) -> Option<&AgentConfigOptionValue> {
        match self {
            CustomAgentServerSettings::Custom {
                default_config_options,
                ..
            }
            | CustomAgentServerSettings::Registry {
                default_config_options,
                ..
            } => default_config_options.get(config_id),
        }
    }

    pub fn favorite_config_option_values(&self, config_id: &str) -> Option<&[String]> {
        match self {
            CustomAgentServerSettings::Custom {
                favorite_config_option_values,
                ..
            }
            | CustomAgentServerSettings::Registry {
                favorite_config_option_values,
                ..
            } => favorite_config_option_values
                .get(config_id)
                .map(|v| v.as_slice()),
        }
    }

    /// ADR-F55: whether the user turned a subscription sign-in off for this
    /// agent. `None` where they said nothing, so that the caller decides what
    /// saying nothing means rather than this answering for it.
    pub fn allow_subscription_sign_in(&self) -> Option<bool> {
        match self {
            CustomAgentServerSettings::Custom {
                allow_subscription_sign_in,
                ..
            }
            | CustomAgentServerSettings::Registry {
                allow_subscription_sign_in,
                ..
            } => *allow_subscription_sign_in,
        }
    }

    /// ADR-F49 decision 19: `run_outside_sandbox` as written, `None` where the
    /// user said nothing.
    pub fn run_outside_sandbox(&self) -> Option<bool> {
        match self {
            CustomAgentServerSettings::Custom {
                run_outside_sandbox,
                ..
            }
            | CustomAgentServerSettings::Registry {
                run_outside_sandbox,
                ..
            } => *run_outside_sandbox,
        }
    }
}

impl From<settings::CustomAgentServerSettings> for CustomAgentServerSettings {
    fn from(value: settings::CustomAgentServerSettings) -> Self {
        match value {
            settings::CustomAgentServerSettings::Custom {
                path,
                args,
                env,
                default_mode,
                default_config_options,
                favorite_config_option_values,
                allow_subscription_sign_in,
                run_outside_sandbox,
            } => CustomAgentServerSettings::Custom {
                command: AgentServerCommand {
                    path: PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref()),
                    args,
                    env: Some(env),
                    sandbox: None,
                },
                default_mode,
                default_config_options,
                favorite_config_option_values,
                allow_subscription_sign_in,
                run_outside_sandbox,
            },
            settings::CustomAgentServerSettings::Registry {
                env,
                default_mode,
                default_config_options,
                favorite_config_option_values,
                allow_subscription_sign_in,
                run_outside_sandbox,
            } => CustomAgentServerSettings::Registry {
                env,
                default_mode,
                default_config_options,
                favorite_config_option_values,
                allow_subscription_sign_in,
                run_outside_sandbox,
            },
        }
    }
}

impl settings::Settings for AllAgentServersSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let agent_settings = content.agent_servers.clone().unwrap();
        Self(
            agent_settings
                .0
                .into_iter()
                .map(|(k, v)| {
                    (
                        EXTENSION_TO_REGISTRY_IDS
                            .get(&k.as_str())
                            .map(|v| v.to_string())
                            .unwrap_or(k),
                        v.into(),
                    )
                })
                .collect(),
        )
    }
}

/// ADR-F49 decision 18 (C): the most processes one stop request may name.
/// What a client asks to stop comes from a list it walked out of a single PID
/// namespace a moment earlier, and a sandbox never holds anything like this
/// many; the bound only keeps a malformed or hostile request from turning into
/// an unbounded number of `/proc` reads and namespace walks on the host.
const MAX_STOPPED_SANDBOX_PROCESSES: usize = 1024;

/// What a stop request names, as the host acts on it. The whole request is
/// refused above [`MAX_STOPPED_SANDBOX_PROCESSES`] rather than truncated: a
/// client that asked for more than a sandbox can hold is not one whose list
/// this host should be acting on at all, and a half-honoured stop would report
/// a number the caller could not tell from a complete one.
fn sandbox_process_keys(
    processes: Vec<proto::SandboxProcessKey>,
) -> Result<Vec<sandbox::SandboxProcessKey>> {
    anyhow::ensure!(
        processes.len() <= MAX_STOPPED_SANDBOX_PROCESSES,
        "a request to stop {} processes of one sandbox names more than the \
         {MAX_STOPPED_SANDBOX_PROCESSES} a sandbox's own list can hold",
        processes.len(),
    );
    Ok(processes
        .into_iter()
        .map(|process| sandbox::SandboxProcessKey {
            pid: process.pid,
            start_time: process.start_time,
        })
        .collect())
}

/// ADR-F49 decision 18 (C): the sandbox sessions this account registered, and
/// what runs in them. Only the host a sandbox was built on can answer, so this
/// lives where the remote server runs.
#[cfg(target_os = "linux")]
fn list_sandbox_processes_on_this_host(session_id: &str) -> Result<Vec<sandbox::SandboxProcess>> {
    let directory = sandbox::sandbox_processes::sandbox_sessions_directory_for_this_account()
        .context("this account has no home directory, so no sandbox session is registered")?;
    sandbox::sandbox_processes::list_sandbox_session_processes(&directory, session_id)
}

#[cfg(target_os = "linux")]
fn stop_sandbox_processes_on_this_host(
    session_id: &str,
    processes: &[sandbox::SandboxProcessKey],
) -> Result<usize> {
    let directory = sandbox::sandbox_processes::sandbox_sessions_directory_for_this_account()
        .context("this account has no home directory, so no sandbox session is registered")?;
    sandbox::sandbox_processes::stop_sandbox_session_processes(&directory, session_id, processes)
}

/// A host whose sandbox has no process boundary this side can look through
/// (macOS) answers that it cannot follow them; the client then keeps the agent
/// a writer for its whole session (ADR-F49 decision 18, case B).
#[cfg(not(target_os = "linux"))]
fn list_sandbox_processes_on_this_host(_session_id: &str) -> Result<Vec<sandbox::SandboxProcess>> {
    bail!("this host cannot follow the processes of a sandbox")
}

#[cfg(not(target_os = "linux"))]
fn stop_sandbox_processes_on_this_host(
    _session_id: &str,
    _processes: &[sandbox::SandboxProcessKey],
) -> Result<usize> {
    bail!("this host cannot follow the processes of a sandbox")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use crate::worktree_store::{WorktreeIdCounter, WorktreeStore};
    use gpui::TestAppContext;
    #[cfg(feature = "test-support")]
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use node_runtime::NodeRuntime;
    use settings::Settings as _;

    #[cfg(feature = "test-support")]
    const TEST_ARCHIVE_URL: &str = "https://example.test/agent";

    /// ADR-F53 決定 4: an `ANTHROPIC_API_KEY` the user put in
    /// `agent_servers.<id>.env` reaches the agent, whatever the IDE would have
    /// set for the same name — upstream blanked it there to push the agent
    /// onto its OAuth sign-in, which this fork does not offer. Every route
    /// that starts a registry agent composes its environment here, so the
    /// order is checked once.
    #[test]
    fn the_agents_own_settings_win_over_what_the_ide_adds() {
        let env = agent_environment(
            HashMap::from_iter([
                ("PATH".to_string(), "/opt/agent/bin".to_string()),
                (
                    "ANTHROPIC_API_KEY".to_string(),
                    "from-distribution".to_string(),
                ),
            ]),
            HashMap::from_iter([
                ("ANTHROPIC_API_KEY".to_string(), String::new()),
                ("NO_BROWSER".to_string(), "1".to_string()),
            ]),
            HashMap::from_iter([("ANTHROPIC_API_KEY".to_string(), "sk-user".to_string())]),
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-user"),
            "the empty default the IDE would add must not win"
        );
        assert_eq!(env.get("PATH").map(String::as_str), Some("/opt/agent/bin"));
        assert_eq!(env.get("NO_BROWSER").map(String::as_str), Some("1"));

        let without_settings = agent_environment(
            HashMap::default(),
            HashMap::from_iter([(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/data/claude-acp-config".to_string(),
            )]),
            HashMap::default(),
        );
        assert_eq!(
            without_settings
                .get("CLAUDE_CONFIG_DIR")
                .map(String::as_str),
            Some("/data/claude-acp-config"),
            "what the IDE adds still reaches an agent whose settings say nothing"
        );
    }

    /// 8c (the rest of ADR-F55): an agent this host starts outside its sandbox
    /// is pointed at no folder of the fork's — it uses the account's own, as
    /// the official CLI there does — while a folder the user spelled out in the
    /// agent's settings is their own word and stays.
    #[test]
    fn the_host_route_names_no_configuration_folder_of_its_own() {
        for command_env in [None, Some(HashMap::default())] {
            let env = host_route_environment(command_env);
            assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "{env:?}");
        }

        let from_settings = host_route_environment(Some(HashMap::from_iter([(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/srv/claude".to_string(),
        )])));
        assert_eq!(
            from_settings.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/srv/claude"),
            "a folder the user spelled out is their own word"
        );
    }

    /// ADR-F88: on a remote host's sandbox too, the API key the client read
    /// from its credential store only fills in what the host resolved.
    #[test]
    fn a_remote_sandbox_keeps_the_hosts_own_api_key() {
        let projects = sandbox::SANDBOXED_AGENT_PROJECTS_HOME_RELATIVE;
        let remote_sandbox = |env: &[(&str, &str)]| proto::AgentServerSandbox {
            config_dir: format!("/home/u/{projects}/0123456789abcdef/config"),
            env: env
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            readable_paths: Vec::new(),
            hosts: Vec::new(),
            hosts_from_editor_allowlist: true,
        };
        let extra_env = HashMap::from_iter([(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-client-stored".to_string(),
        )]);

        let accepted = accept_remote_agent_sandbox(
            &SandboxedAgentProfile::Claude,
            remote_sandbox(&[("ANTHROPIC_API_KEY", "sk-host")]),
            &extra_env,
        )
        .expect("the folder the profile names");
        assert_eq!(
            accepted.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-host")
        );

        let accepted = accept_remote_agent_sandbox(
            &SandboxedAgentProfile::Claude,
            remote_sandbox(&[]),
            &extra_env,
        )
        .expect("the folder the profile names");
        assert_eq!(
            accepted.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-client-stored")
        );
    }

    /// ADR-F88: the card takes a blank key in the settings as no key, so the
    /// agent must get the stored one rather than the blank.
    #[test]
    fn a_blank_key_in_the_settings_does_not_shadow_the_stored_one() {
        let stored =
            HashMap::from_iter([("ANTHROPIC_API_KEY".to_string(), "sk-stored".to_string())]);
        let blank = HashMap::from_iter([
            ("ANTHROPIC_API_KEY".to_string(), " ".to_string()),
            ("OTHER".to_string(), String::new()),
        ]);

        let env = agent_environment(HashMap::default(), stored.clone(), blank.clone());
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-stored")
        );
        assert_eq!(
            env.get("OTHER").map(String::as_str),
            Some(""),
            "other blank values are the user's and stay"
        );

        let mut env = blank;
        apply_extra_env(&mut env, stored);
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-stored")
        );
    }

    /// ADR-F88: only the stored API key is held back; everything else the
    /// client adds goes over the agent's resolved environment, as upstream.
    #[test]
    fn only_a_stored_api_key_fills_in() {
        let mut env = HashMap::from_iter([
            ("ANTHROPIC_API_KEY".to_string(), "sk-own".to_string()),
            ("NO_BROWSER".to_string(), "0".to_string()),
        ]);
        apply_extra_env(
            &mut env,
            HashMap::from_iter([
                ("ANTHROPIC_API_KEY".to_string(), "sk-stored".to_string()),
                ("NO_BROWSER".to_string(), "1".to_string()),
                ("HTTPS_PROXY".to_string(), "http://127.0.0.1:1".to_string()),
            ]),
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-own")
        );
        assert_eq!(env.get("NO_BROWSER").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("HTTPS_PROXY").map(String::as_str),
            Some("http://127.0.0.1:1")
        );

        let mut env = HashMap::default();
        apply_extra_env(
            &mut env,
            HashMap::from_iter([("ANTHROPIC_API_KEY".to_string(), "sk-stored".to_string())]),
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-stored")
        );
    }

    /// ADR-F88: a custom agent's own `env` is the user's word, so an API key
    /// spelled out there wins over the stored one the client adds — inside
    /// the WSL sandbox as outside it.
    #[cfg(all(target_os = "windows", feature = "test-support"))]
    #[gpui::test]
    async fn a_custom_agents_own_api_key_wins_over_the_stored_one(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let extra_env = || {
            HashMap::from_iter([
                ("ANTHROPIC_API_KEY".to_string(), "sk-stored".to_string()),
                ("NO_BROWSER".to_string(), "1".to_string()),
            ])
        };
        let sandbox_env = |agent: &mut LocalCustomAgent, cx: &mut TestAppContext| {
            cx.update(|cx| {
                agent.get_command(
                    Vec::new(),
                    extra_env(),
                    AgentLaunchTarget::Sandbox {
                        wsl_distro: Some("Ubuntu".to_string()),
                    },
                    &mut cx.to_async(),
                )
            })
        };

        let mut agent = make_custom_agent(cx, "/usr/local/bin/my-agent");
        agent
            .command
            .env
            .get_or_insert_default()
            .insert("ANTHROPIC_API_KEY".to_string(), "sk-settings".to_string());
        let sandbox = sandbox_env(&mut agent, cx)
            .await
            .expect("a Linux command")
            .sandbox
            .expect("a sandbox part");
        assert_eq!(
            sandbox.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-settings")
        );
        assert_eq!(sandbox.env.get("NO_BROWSER").map(String::as_str), Some("1"));

        let mut agent = make_custom_agent(cx, "/usr/local/bin/my-agent");
        let sandbox = sandbox_env(&mut agent, cx)
            .await
            .expect("a Linux command")
            .sandbox
            .expect("a sandbox part");
        assert_eq!(
            sandbox.env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-stored"),
            "with none in the settings, the stored key reaches the agent"
        );
    }

    /// ADR-F49 decision 18 (C): each named process costs a `/proc` read and a
    /// namespace walk on the host before it is signalled, so a request far
    /// above what one sandbox can hold is refused whole.
    #[test]
    fn a_stop_request_is_refused_above_what_a_sandbox_can_hold() {
        let keys = |count: usize| {
            (0..count)
                .map(|index| proto::SandboxProcessKey {
                    pid: index as u32 + 1,
                    start_time: 1,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(sandbox_process_keys(keys(0)).unwrap().len(), 0);
        let accepted = sandbox_process_keys(keys(MAX_STOPPED_SANDBOX_PROCESSES))
            .expect("the most a sandbox's list can hold is still acted on");
        assert_eq!(accepted.len(), MAX_STOPPED_SANDBOX_PROCESSES);
        assert_eq!(
            accepted.first(),
            Some(&sandbox::SandboxProcessKey {
                pid: 1,
                start_time: 1
            })
        );
        assert!(
            sandbox_process_keys(keys(MAX_STOPPED_SANDBOX_PROCESSES + 1)).is_err(),
            "one more than that is refused rather than truncated"
        );
    }

    #[cfg(feature = "test-support")]
    fn static_http_client(body: Vec<u8>) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |_| {
            let body = body.clone();
            async move {
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))?)
            }
        })
    }

    #[cfg(feature = "test-support")]
    fn make_registry_archive_agent(
        cx: &mut TestAppContext,
        installation_dir: PathBuf,
        http_client: Arc<dyn HttpClient>,
        sha256: Option<String>,
    ) -> LocalRegistryArchiveAgent {
        let fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.executor());
        let target = RegistryTargetConfig {
            archive: TEST_ARCHIVE_URL.to_string(),
            cmd: "./agent".to_string(),
            args: Vec::new(),
            sha256,
            env: HashMap::default(),
        };
        let targets = [
            "darwin-aarch64",
            "darwin-x86_64",
            "linux-aarch64",
            "linux-x86_64",
            "windows-aarch64",
            "windows-x86_64",
        ]
        .into_iter()
        .map(|platform| (platform.to_string(), target.clone()))
        .collect();

        cx.update(|cx| {
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));
            let project_environment = cx.new(|cx| {
                crate::ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx)
            });

            LocalRegistryArchiveAgent {
                fs,
                http_client,
                node_runtime: NodeRuntime::unavailable(),
                project_environment,
                registry_id: Arc::from("test-agent"),
                installation_dir,
                version: "1.0.0".into(),
                targets,
                env: HashMap::default(),
                update_approvals: AgentUpdateApprovals::default(),
                new_version_available_tx: NewVersionSlot::default(),
                loading_status_tx: None,
            }
        })
    }

    /// ADR-F49 決定 19: an agent the host could not place in its sandbox keeps
    /// its command but is described as unavailable, so the client refuses to
    /// start it and offers to start it outside the sandbox once.
    #[test]
    fn a_sandbox_that_could_not_be_built_is_reported_rather_than_skipped() {
        let sandbox = AgentSandboxCommand::new(
            SandboxedAgentProfile::for_agent("claude-acp", true),
            HashMap::default(),
            Vec::new(),
        );
        assert!(matches!(
            sandbox_placement_failed(sandbox.clone(), None).state,
            AgentSandboxState::Unresolved { .. }
        ));
        let failed = sandbox_placement_failed(sandbox, Some("WSL がありません".to_string()));
        assert_eq!(
            failed.state,
            AgentSandboxState::Unavailable {
                reason: "WSL がありません".to_string()
            }
        );
    }

    #[cfg(all(target_os = "windows", feature = "test-support"))]
    fn make_custom_agent(cx: &mut TestAppContext, command_path: &str) -> LocalCustomAgent {
        let fs: Arc<dyn Fs> = fs::RealFs::new(None, cx.executor());
        cx.update(|cx| {
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));
            let project_environment = cx.new(|cx| {
                crate::ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx)
            });
            LocalCustomAgent {
                project_environment,
                agent_id: Arc::from("my-agent"),
                command: AgentServerCommand {
                    path: PathBuf::from(command_path),
                    args: vec!["--acp".to_string()],
                    env: Some(HashMap::from_iter([(
                        "MY_AGENT_TOKEN".to_string(),
                        "t".to_string(),
                    )])),
                    sandbox: None,
                },
            }
        })
    }

    /// ADR-F49 第 2b 段: a custom agent spelled as a Linux absolute path is a
    /// command inside the project's WSL distro, which is what the client
    /// starts it as; its Windows environment is dropped, because none of it
    /// means anything there.
    #[cfg(all(target_os = "windows", feature = "test-support"))]
    #[gpui::test]
    async fn a_linux_custom_agent_is_a_command_inside_the_distro(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let mut agent = make_custom_agent(cx, "/usr/local/bin/my-agent");
        let command = cx
            .update(|cx| {
                agent.get_command(
                    Vec::new(),
                    HashMap::default(),
                    AgentLaunchTarget::Sandbox {
                        wsl_distro: Some("Ubuntu".to_string()),
                    },
                    &mut cx.to_async(),
                )
            })
            .await
            .expect("a Linux command");

        let sandbox = command.sandbox.expect("a sandbox part");
        assert_eq!(
            sandbox.wsl,
            Some(crate::agent_sandbox::WslAgentRuntime {
                distro: Some("Ubuntu".to_string())
            })
        );
        assert!(matches!(
            sandbox.state,
            AgentSandboxState::Unresolved { .. }
        ));
        assert_eq!(
            sandbox.env.get("MY_AGENT_TOKEN").map(String::as_str),
            Some("t"),
            "the agent keeps its own environment inside the sandbox"
        );
        assert_eq!(
            command.env, None,
            "a Linux command takes none of this machine's environment"
        );
        assert_eq!(command.path, PathBuf::from("/usr/local/bin/my-agent"));
    }

    /// ADR-F49 第 2b 段 / 決定 19: a custom agent spelled as a Windows program
    /// cannot run in the WSL sandbox, and says so in Japanese rather than
    /// being started outside it without a word.
    #[cfg(all(target_os = "windows", feature = "test-support"))]
    #[gpui::test]
    async fn a_windows_custom_agent_cannot_run_in_the_distro(cx: &mut TestAppContext) {
        init_test_settings(cx);
        for windows_path in [
            "C:\\tools\\my-agent.exe",
            "\\\\server\\share\\my-agent.exe",
            "my-agent.exe",
        ] {
            let mut agent = make_custom_agent(cx, windows_path);
            let command = cx
                .update(|cx| {
                    agent.get_command(
                        Vec::new(),
                        HashMap::default(),
                        AgentLaunchTarget::Sandbox { wsl_distro: None },
                        &mut cx.to_async(),
                    )
                })
                .await
                .expect("a command with a reason");

            let sandbox = command.sandbox.expect("a sandbox part");
            assert_eq!(sandbox.wsl, None, "{windows_path} is no Linux command");
            let AgentSandboxState::Unavailable { reason } = sandbox.state else {
                panic!("{windows_path} should not be placed in the sandbox");
            };
            assert!(
                reason.contains(windows_path) && reason.contains("WSL"),
                "the reason names the program and the sandbox: {reason}"
            );
            assert!(
                command.env.is_some(),
                "the host command keeps the host environment"
            );
        }
    }

    /// The start the user confirmed outside the sandbox (決定 19) is what it
    /// was before 第 2b 段: the host's own command, with nothing of WSL in it.
    #[cfg(all(target_os = "windows", feature = "test-support"))]
    #[gpui::test]
    async fn the_host_target_is_unchanged(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let mut agent = make_custom_agent(cx, "/usr/local/bin/my-agent");
        let command = cx
            .update(|cx| {
                agent.get_command(
                    Vec::new(),
                    HashMap::default(),
                    AgentLaunchTarget::Host,
                    &mut cx.to_async(),
                )
            })
            .await
            .expect("the host command");

        let sandbox = command.sandbox.expect("a sandbox part");
        assert_eq!(sandbox.wsl, None);
        assert!(matches!(
            sandbox.state,
            AgentSandboxState::Unresolved { .. }
        ));
        assert!(command.env.is_some());
    }

    /// ADR-F49 決定 19: when the project's WSL distro cannot take the agent —
    /// here because it is not registered at all — the host's own command comes
    /// back *with the reason*, so the client refuses to start it and offers to
    /// start it outside the sandbox once. Nothing is started unsandboxed
    /// without a word, and nothing of WSL is claimed.
    #[cfg(all(target_os = "windows", feature = "test-support"))]
    #[gpui::test]
    async fn a_registry_archive_says_why_the_distro_could_not_take_it(cx: &mut TestAppContext) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let contents = b"verified agent";
        let expected_sha256 = format!("{:X}", Sha256::digest(contents));
        let http_client = static_http_client(contents.to_vec());
        let mut agent = make_registry_archive_agent(
            cx,
            installation_dir.clone(),
            http_client,
            Some(expected_sha256.clone()),
        );
        let get_command = cx.update(|cx| {
            agent.get_command(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Sandbox {
                    // No distro is ever registered under this name, so the
                    // WSL half fails before it spawns anything.
                    wsl_distro: Some("zed-jp-enhance-no-such-distro".to_string()),
                },
                &mut cx.to_async(),
            )
        });

        let command = get_command.await.expect("the host command and a reason");
        cx.run_until_parked();
        let sandbox = command.sandbox.expect("a sandbox part");
        assert_eq!(sandbox.wsl, None, "nothing was placed in a distro");
        let AgentSandboxState::Unavailable { reason } = sandbox.state else {
            panic!("an unregistered distro cannot take the agent");
        };
        assert!(
            reason.contains("zed-jp-enhance-no-such-distro"),
            "the reason names the distro: {reason}"
        );
        assert_eq!(
            command.path,
            versioned_archive_cache_dir(
                &installation_dir,
                Some("1.0.0"),
                TEST_ARCHIVE_URL,
                Some(&expected_sha256),
            )
            .join("agent"),
            "the command that comes back is this host's own"
        );
        assert!(
            command.env.is_some(),
            "the host command keeps the host environment"
        );
    }

    /// ADR-F49 第 2b 段: the `sandbox` crate deliberately keeps no timer of
    /// its own, so a `wsl.exe` that never answers — what an unhealthy WSL
    /// service does — is bounded here. The step is cut off with an
    /// explanation, and the future is dropped, which is what kills the
    /// `wsl.exe` it left running.
    #[cfg(target_os = "windows")]
    #[gpui::test]
    async fn a_step_inside_wsl_that_never_answers_is_cut_off(cx: &mut TestAppContext) {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct KilledOnDrop(Arc<AtomicBool>);

        impl Drop for KilledOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let wedged = {
            let dropped = dropped.clone();
            async move {
                let _killed = KilledOnDrop(dropped);
                futures::future::pending::<anyhow::Result<()>>().await
            }
        };

        let error = wsl_runtime::bounded(
            &cx.executor(),
            wsl_runtime::STEP_TIMEOUT,
            "Checking Node.js in WSL",
            wedged,
        )
        .await
        .expect_err("a wedged wsl.exe must be cut off");

        let message = format!("{error:#}");
        assert!(
            message.contains("Checking Node.js in WSL did not finish")
                && message.contains(&wsl_runtime::STEP_TIMEOUT.as_secs().to_string())
                && message.contains("wsl --shutdown"),
            "{message}"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the step must be dropped, which is what kills the wsl.exe"
        );
    }

    /// ADR-F49 決定 21: a runtime on its way into a distro is downloaded
    /// outside `node/`, which this host's own `ManagedNodeRuntime` removes
    /// whole when it reinstalls Node.js for itself — that would take a
    /// download in progress with it.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_runtime_is_downloaded_outside_what_a_node_reinstall_empties() {
        let downloads = wsl_runtime::downloads_directory();
        assert!(
            downloads.starts_with(paths::external_agents_dir()),
            "{} is not below the external agents directory",
            downloads.display()
        );
        assert!(
            !downloads.starts_with(paths::data_dir().join("node")),
            "{} is inside the directory a Node.js reinstall empties",
            downloads.display()
        );
    }

    fn make_npx_agent(id: &str, version: &str) -> RegistryAgent {
        let id = SharedString::from(id.to_string());
        RegistryAgent::Npx(RegistryNpxAgent {
            metadata: RegistryAgentMetadata {
                id: AgentId::new(id.clone()),
                name: id.clone(),
                description: SharedString::from(""),
                version: SharedString::from(version.to_string()),
                repository: None,
                website: None,
                license_url: None,
                icon_path: None,
            },
            package: id,
            args: Vec::new(),
            env: HashMap::default(),
        })
    }

    fn init_test_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn init_registry(
        cx: &mut TestAppContext,
        agents: Vec<RegistryAgent>,
    ) -> gpui::Entity<AgentRegistryStore> {
        cx.update(|cx| AgentRegistryStore::init_test_global(cx, agents))
    }

    fn set_registry_settings(cx: &mut TestAppContext, agent_names: &[&str]) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                AllAgentServersSettings(
                    agent_names
                        .iter()
                        .map(|name| {
                            (
                                name.to_string(),
                                settings::CustomAgentServerSettings::Registry {
                                    env: HashMap::default(),
                                    default_mode: None,
                                    default_config_options: HashMap::default(),
                                    favorite_config_option_values: HashMap::default(),
                                    allow_subscription_sign_in: None,
                                    run_outside_sandbox: None,
                                }
                                .into(),
                            )
                        })
                        .collect(),
                ),
                cx,
            );
        });
    }

    fn create_agent_server_store(cx: &mut TestAppContext) -> gpui::Entity<AgentServerStore> {
        cx.update(|cx| {
            let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor().clone());
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));
            let project_environment = cx.new(|cx| {
                crate::ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx)
            });
            let http_client = http_client::FakeHttpClient::with_404_response();

            cx.new(|cx| {
                AgentServerStore::local(
                    NodeRuntime::unavailable(),
                    fs,
                    project_environment,
                    http_client,
                    cx,
                )
            })
        })
    }

    #[test]
    fn builds_bounded_npm_package_specs() {
        assert_eq!(
            bounded_npm_package_spec("agent-package@1.2.3"),
            ("agent-package", "agent-package@0.0.0 - 1.2.3".to_string())
        );
        assert_eq!(
            bounded_npm_package_spec("@scope/agent-package@1.2.3-beta.1"),
            (
                "@scope/agent-package",
                "@scope/agent-package@0.0.0 - 1.2.3-beta.1".to_string()
            )
        );
        assert_eq!(
            bounded_npm_package_spec("@scope/agent-package"),
            ("@scope/agent-package", "@scope/agent-package".to_string())
        );
        assert_eq!(
            bounded_npm_package_spec("agent-package@latest"),
            ("agent-package", "agent-package@latest".to_string())
        );
    }

    #[test]
    fn detects_supported_archive_suffixes() {
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.zip"),
            Ok(RegistryArchiveKind::Archive(AssetKind::Zip))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.zip?download=1"),
            Ok(RegistryArchiveKind::Archive(AssetKind::Zip))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tar.gz"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarGz))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tar.gz?download=1#latest"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarGz))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tgz"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarGz))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tgz#download"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarGz))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tar.bz2"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarBz2))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tar.bz2?download=1"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarBz2))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tbz2"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarBz2))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.tbz2#download"),
            Ok(RegistryArchiveKind::Archive(AssetKind::TarBz2))
        ));
        assert!(matches!(
            registry_archive_kind_for_url("https://example.com/agent.ZIP"),
            Ok(RegistryArchiveKind::Archive(AssetKind::Zip))
        ));
    }

    #[test]
    fn detects_raw_binary_archive_urls() {
        assert_eq!(
            registry_archive_kind_for_url("https://x.ai/cli/grok-0.2.20-macos-aarch64").unwrap(),
            RegistryArchiveKind::RawBinary {
                file_name: "grok-0.2.20-macos-aarch64".to_string()
            },
        );
        assert_eq!(
            registry_archive_kind_for_url("https://x.ai/cli/grok-0.2.20-windows-x86_64.exe")
                .unwrap(),
            RegistryArchiveKind::RawBinary {
                file_name: "grok-0.2.20-windows-x86_64.exe".to_string()
            },
        );
        assert_eq!(
            registry_archive_kind_for_url("https://example.com/agent-binary?download=1#latest")
                .unwrap(),
            RegistryArchiveKind::RawBinary {
                file_name: "agent-binary".to_string()
            },
        );
        assert_eq!(
            registry_archive_kind_for_url("https://example.com/agent%20binary").unwrap(),
            RegistryArchiveKind::RawBinary {
                file_name: "agent binary".to_string()
            },
        );
        // No file name to install the binary as.
        assert!(registry_archive_kind_for_url("https://example.com/").is_err());
        // Percent-decoding must not allow path traversal in the file name.
        assert!(registry_archive_kind_for_url("https://example.com/a%2F..%2Fevil").is_err());
        assert!(registry_archive_kind_for_url("https://example.com/%2E%2E").is_err());
    }

    #[test]
    fn parses_github_release_archive_urls() {
        let github_archive = github_release_archive_from_url(
            "https://github.com/owner/repo/releases/download/release%2F2.3.5/agent.tar.bz2?download=1",
        )
        .unwrap();

        assert_eq!(github_archive.repo_name_with_owner, "owner/repo");
        assert_eq!(github_archive.tag, "release/2.3.5");
        assert_eq!(github_archive.asset_name, "agent.tar.bz2");
    }

    #[test]
    fn rejects_unsupported_archive_suffixes() {
        let error = registry_archive_kind_for_url("https://example.com/agent.tar.xz")
            .err()
            .map(|error| error.to_string());

        assert_eq!(
            error,
            Some(
                "unsupported archive type .tar.xz in URL: https://example.com/agent.tar.xz"
                    .to_string()
            ),
        );

        for installer_url in [
            "https://example.com/agent.dmg",
            "https://example.com/agent.pkg",
            "https://example.com/agent.deb",
            "https://example.com/agent.rpm",
            "https://example.com/agent.msi",
            "https://example.com/agent.AppImage",
        ] {
            assert!(
                registry_archive_kind_for_url(installer_url).is_err(),
                "expected {installer_url} to be rejected"
            );
        }
    }

    #[test]
    fn versioned_archive_cache_dir_includes_artifact_identity() {
        let slash_version_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release/2.3.5"),
            "https://example.com/agent.zip",
            None,
        );
        let colon_version_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release:2.3.5"),
            "https://example.com/agent.zip",
            None,
        );
        let file_name = slash_version_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("cache directory should have a file name");

        assert!(file_name.starts_with("v_release-2.3.5_"));
        assert_ne!(slash_version_dir, colon_version_dir);

        let lowercase_checksum_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release/2.3.5"),
            "https://example.com/agent.zip",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        let uppercase_checksum_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release/2.3.5"),
            "https://example.com/agent.zip",
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        );
        let changed_checksum_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release/2.3.5"),
            "https://example.com/agent.zip",
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        );

        assert_ne!(slash_version_dir, lowercase_checksum_dir);
        assert_eq!(lowercase_checksum_dir, uppercase_checksum_dir);
        assert_ne!(lowercase_checksum_dir, changed_checksum_dir);
    }

    #[cfg(feature = "test-support")]
    #[gpui::test]
    async fn registry_raw_binary_checksum_invalidates_unverified_cache_and_blocks_mismatch(
        cx: &mut TestAppContext,
    ) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let old_version_dir =
            versioned_archive_cache_dir(&installation_dir, Some("1.0.0"), TEST_ARCHIVE_URL, None);
        std::fs::create_dir_all(&old_version_dir).unwrap();
        std::fs::write(old_version_dir.join("agent"), b"unverified agent").unwrap();

        let expected_sha256 = "0000000000000000000000000000000000000000000000000000000000000000";
        let http_client = static_http_client(b"unexpected agent".to_vec());
        let mut agent = make_registry_archive_agent(
            cx,
            installation_dir.clone(),
            http_client,
            Some(expected_sha256.to_string()),
        );
        let get_command = cx.update(|cx| {
            agent.get_command(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Host,
                &mut cx.to_async(),
            )
        });

        let error = get_command.await.unwrap_err();
        assert!(
            error.to_string().contains("SHA-256 mismatch"),
            "unexpected error: {error:#}"
        );
        assert!(old_version_dir.exists());
        assert!(
            !versioned_archive_cache_dir(
                &installation_dir,
                Some("1.0.0"),
                TEST_ARCHIVE_URL,
                Some(expected_sha256),
            )
            .exists()
        );
    }

    #[cfg(feature = "test-support")]
    #[gpui::test]
    async fn registry_raw_binary_with_checksum_installs(cx: &mut TestAppContext) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let contents = b"verified agent";
        let expected_sha256 = format!("{:X}", Sha256::digest(contents));
        let http_client = static_http_client(contents.to_vec());
        let mut agent = make_registry_archive_agent(
            cx,
            installation_dir.clone(),
            http_client,
            Some(expected_sha256.clone()),
        );
        let get_command = cx.update(|cx| {
            agent.get_command(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Host,
                &mut cx.to_async(),
            )
        });

        let command = get_command.await.unwrap();
        cx.run_until_parked();
        assert_eq!(
            command.path,
            versioned_archive_cache_dir(
                &installation_dir,
                Some("1.0.0"),
                TEST_ARCHIVE_URL,
                Some(&expected_sha256),
            )
            .join("agent")
        );
        assert_eq!(std::fs::read(command.path).unwrap(), contents);
    }

    #[cfg(feature = "test-support")]
    #[gpui::test]
    async fn registry_raw_binary_without_checksum_installs(cx: &mut TestAppContext) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let contents = b"unchecked agent";
        let http_client = static_http_client(contents.to_vec());
        let mut agent =
            make_registry_archive_agent(cx, installation_dir.clone(), http_client, None);
        let get_command = cx.update(|cx| {
            agent.get_command(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Host,
                &mut cx.to_async(),
            )
        });

        let command = get_command.await.unwrap();
        cx.run_until_parked();
        assert_eq!(
            command.path,
            versioned_archive_cache_dir(&installation_dir, Some("1.0.0"), TEST_ARCHIVE_URL, None,)
                .join("agent")
        );
        assert_eq!(std::fs::read(command.path).unwrap(), contents);
    }

    #[gpui::test]
    async fn test_remove_stale_versioned_archive_cache_dirs(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let base_dir = Path::new("/cache");

        // FakeFs increments mtime on every create, so creation order is
        // ascending mtime: v_old_1 < v_old_2 < other < v_not_a_dir < v_current < v_newer.
        fs.insert_tree(
            base_dir,
            serde_json::json!({
                "v_old_1": {},
                "v_old_2": {},
                "other": {},
            }),
        )
        .await;
        fs.insert_file(base_dir.join("v_not_a_dir"), b"keep me".to_vec())
            .await;
        let current_version_dir = base_dir.join("v_current");
        fs.create_dir(&current_version_dir).await.unwrap();
        // Sibling that "finished extracting" after the current dir was cached.
        fs.create_dir(&base_dir.join("v_newer")).await.unwrap();

        remove_stale_versioned_archive_cache_dirs(
            fs.clone() as Arc<dyn Fs>,
            base_dir,
            &current_version_dir,
        )
        .await
        .unwrap();

        let mut remaining = fs
            .read_dir(base_dir)
            .await
            .unwrap()
            .filter_map(|entry| async move { entry.ok() })
            .map(|path| {
                path.file_name()
                    .expect("entry has a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .await;
        remaining.sort();

        assert_eq!(
            remaining,
            vec![
                "other".to_string(),
                "v_current".to_string(),
                "v_newer".to_string(),
                "v_not_a_dir".to_string(),
            ]
        );
    }

    const CACHED_ARCHIVE_URL: &str = "https://example.test/cached-agent";

    #[test]
    fn versioned_archive_dir_version_reads_back_the_cache_dir_name() {
        let version_of = |version: &str| {
            let directory = versioned_archive_cache_dir(
                Path::new("/cache"),
                Some(version),
                CACHED_ARCHIVE_URL,
                None,
            );
            let name = directory
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string);
            name.and_then(|name| versioned_archive_dir_version(&name).map(str::to_string))
        };
        assert_eq!(version_of("1.2.3"), Some("1.2.3".to_string()));
        assert_eq!(
            version_of("2.0.0+build_7"),
            Some("2.0.0-build_7".to_string())
        );

        assert_eq!(versioned_archive_dir_version("v_current"), None);
        assert_eq!(versioned_archive_dir_version("other"), None);
        assert_eq!(
            versioned_archive_dir_version("v_1.0.0_0123456789abcdef_not-a-hash-at-all"),
            None
        );
    }

    #[test]
    fn registry_archive_version_matches_compares_sanitized_versions() {
        assert!(registry_archive_version_matches("1.2.3", "1.2.3"));
        assert!(registry_archive_version_matches(
            "2.0.0-build_7",
            "2.0.0+build_7"
        ));
        assert!(!registry_archive_version_matches("1.2.3", "1.3.0"));
    }

    #[gpui::test]
    async fn test_versioned_archive_versions_newest_first(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let base_dir = Path::new("/cache");

        assert_eq!(
            versioned_archive_versions_newest_first(fs.as_ref(), base_dir).await,
            Vec::<String>::new()
        );

        fs.create_dir(base_dir).await.unwrap();
        assert_eq!(
            versioned_archive_versions_newest_first(fs.as_ref(), base_dir).await,
            Vec::<String>::new()
        );

        // FakeFs increments mtime on every create, so the creation order below
        // is the mtime order, which is neither the name order nor its reverse.
        // The stray entries created after them are newer still but are not
        // version directories.
        for version in ["1.0.0", "3.0.0", "2.0.0"] {
            fs.create_dir(&versioned_archive_cache_dir(
                base_dir,
                Some(version),
                CACHED_ARCHIVE_URL,
                None,
            ))
            .await
            .unwrap();
        }
        fs.create_dir(&base_dir.join("v_unrelated")).await.unwrap();
        let not_a_dir =
            versioned_archive_cache_dir(base_dir, Some("9.9.9"), CACHED_ARCHIVE_URL, None);
        fs.insert_file(&not_a_dir, b"not a version".to_vec()).await;

        assert_eq!(
            versioned_archive_versions_newest_first(fs.as_ref(), base_dir).await,
            vec![
                "2.0.0".to_string(),
                "3.0.0".to_string(),
                "1.0.0".to_string()
            ]
        );
    }

    #[gpui::test]
    fn test_version_change_sends_notification(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let store = create_agent_server_store(cx);

        // Verify the agent was registered with version 1.0.0.
        store.read_with(cx, |store, _| {
            let entry = store
                .external_agents
                .get(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            assert_eq!(
                entry.server.version().map(|v| v.to_string()),
                Some("1.0.0".to_string())
            );
        });

        // Set up a watch channel and store the tx on the agent.
        let (tx, mut rx) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            entry.server.set_new_version_available_tx(tx);
        });

        // Update the registry to version 2.0.0.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "2.0.0")], cx);
        });
        cx.run_until_parked();

        // The watch channel should have received the new version.
        assert_eq!(rx.borrow().as_deref(), Some("2.0.0"));
    }

    #[gpui::test]
    fn test_same_version_preserves_tx(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let store = create_agent_server_store(cx);

        let (tx, mut rx) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            entry.server.set_new_version_available_tx(tx);
        });

        // "Refresh" the registry with the same version.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "1.0.0")], cx);
        });
        cx.run_until_parked();

        // No notification should have been sent.
        assert_eq!(rx.borrow().as_deref(), None);

        // The tx should have been transferred to the rebuilt agent entry.
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            assert!(
                entry.server.take_new_version_available_tx().is_some(),
                "tx should have been transferred to the rebuilt agent"
            );
        });
    }

    #[gpui::test]
    fn test_no_tx_stored_does_not_panic_on_version_change(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let _store = create_agent_server_store(cx);

        // Update the registry without having stored any tx — should not panic.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "2.0.0")], cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn test_multiple_agents_independent_notifications(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(
            cx,
            vec![
                make_npx_agent("agent-a", "1.0.0"),
                make_npx_agent("agent-b", "3.0.0"),
            ],
        );
        set_registry_settings(cx, &["agent-a", "agent-b"]);
        let store = create_agent_server_store(cx);

        let (tx_a, mut rx_a) = watch::channel::<Option<String>>(None);
        let (tx_b, mut rx_b) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            store
                .external_agents
                .get_mut(&AgentId::new("agent-a"))
                .expect("agent-a should be registered")
                .server
                .set_new_version_available_tx(tx_a);
            store
                .external_agents
                .get_mut(&AgentId::new("agent-b"))
                .expect("agent-b should be registered")
                .server
                .set_new_version_available_tx(tx_b);
        });

        // Update only agent-a to a new version; agent-b stays the same.
        registry.update(cx, |store, cx| {
            store.set_agents(
                vec![
                    make_npx_agent("agent-a", "2.0.0"),
                    make_npx_agent("agent-b", "3.0.0"),
                ],
                cx,
            );
        });
        cx.run_until_parked();

        // agent-a should have received a notification.
        assert_eq!(rx_a.borrow().as_deref(), Some("2.0.0"));

        // agent-b should NOT have received a notification.
        assert_eq!(rx_b.borrow().as_deref(), None);

        // agent-b's tx should have been transferred.
        store.update(cx, |store, _| {
            assert!(
                store
                    .external_agents
                    .get_mut(&AgentId::new("agent-b"))
                    .expect("agent-b should be registered")
                    .server
                    .take_new_version_available_tx()
                    .is_some(),
                "agent-b tx should have been transferred"
            );
        });
    }

    const NPX_INSTALL_DIR: &str = "/agents/npx/test-agent";

    /// Runs [`resolve_host_npx_install`] with a stand-in for `npm install`
    /// that counts its runs and puts the package's executable in place.
    async fn resolve_npx(
        fs: &Arc<fs::FakeFs>,
        installs: &Arc<std::sync::atomic::AtomicUsize>,
        registry_version: &str,
        update_approved: bool,
    ) -> (HostNpxLaunch, Option<String>) {
        let install_dir = PathBuf::from(NPX_INSTALL_DIR);
        resolve_host_npx_install(
            fs.as_ref(),
            &install_dir,
            registry_version,
            &format!("test-agent@{registry_version}"),
            &[format!("--from-{registry_version}")],
            &HashMap::default(),
            update_approved,
            async |package_spec: String, package_name: String| {
                assert_eq!(package_name, "test-agent");
                installs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let version = package_spec
                    .rsplit(' ')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let package_dir = install_dir.join("node_modules").join("test-agent");
                fs.create_dir(&package_dir).await?;
                let executable = package_dir.join(format!("{version}.js"));
                fs.insert_file(&executable, Vec::new()).await;
                Ok(executable)
            },
        )
        .await
        .expect("resolves")
    }

    fn npx_installs(installs: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
        installs.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn recorded_version(fs: &Arc<fs::FakeFs>, record_path: &Path) -> Option<String> {
        read_installed_record(fs.as_ref(), record_path)
            .await
            .map(|record| record.registry_version)
    }

    /// ADR-F99: the first start installs and records; every later start with
    /// that record launches it without running `npm install` again.
    #[gpui::test]
    async fn an_npx_agent_with_a_record_starts_without_npm_install(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let installs = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let (first, newer) = resolve_npx(&fs, &installs, "1.0.0", false).await;
        assert_eq!(npx_installs(&installs), 1);
        assert_eq!(newer, None);
        assert_eq!(
            recorded_version(
                &fs,
                &Path::new(NPX_INSTALL_DIR).join(INSTALLED_RECORD_FILE_NAME)
            )
            .await
            .as_deref(),
            Some("1.0.0")
        );

        // ADR-F99: a start that launches the recorded install says when.
        let record_path = Path::new(NPX_INSTALL_DIR).join(INSTALLED_RECORD_FILE_NAME);
        let mut record = read_installed_record(fs.as_ref(), &record_path)
            .await
            .expect("recorded");
        assert!(record.last_started.is_some(), "the install says when");
        record.last_started = Some(1);
        write_installed_record(fs.as_ref(), &record_path, &record)
            .await
            .unwrap();

        let (second, newer) = resolve_npx(&fs, &installs, "1.0.0", false).await;
        assert!(
            read_installed_record(fs.as_ref(), &record_path)
                .await
                .and_then(|record| record.last_started)
                .is_some_and(|started| started > 1),
            "the start that launched the recorded install wrote when"
        );
        assert_eq!(
            npx_installs(&installs),
            1,
            "no npm install on the second start"
        );
        assert_eq!(newer, None);
        assert_eq!(second, first, "the recorded install, as it was");
    }

    /// ADR-F99: a record whose executable is gone is no record.
    #[gpui::test]
    async fn an_npx_agent_whose_executable_is_gone_is_installed_again(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let installs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (first, _) = resolve_npx(&fs, &installs, "1.0.0", false).await;
        fs.remove_file(&first.executable, Default::default())
            .await
            .unwrap();

        let (second, _) = resolve_npx(&fs, &installs, "1.0.0", false).await;
        assert_eq!(npx_installs(&installs), 2);
        assert!(fs.is_file(&second.executable).await);
    }

    async fn put_file(fs: &Arc<fs::FakeFs>, path: &Path) {
        if let Some(parent) = path.parent() {
            fs.create_dir(parent).await.unwrap();
        }
        fs.insert_file(path, Vec::new()).await;
    }

    /// ADR-F99: a record is a file on disk, so one that names a program
    /// outside the install it describes is no record: the start installs
    /// and launches what npm put there.
    #[gpui::test]
    async fn an_npx_record_naming_a_program_elsewhere_is_no_record(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let installs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let record_path = Path::new(NPX_INSTALL_DIR).join(INSTALLED_RECORD_FILE_NAME);
        for elsewhere in [
            "/elsewhere/payload.js".to_string(),
            format!("{NPX_INSTALL_DIR}/node_modules/../../payload.js"),
        ] {
            put_file(&fs, Path::new("/elsewhere/payload.js")).await;
            put_file(&fs, Path::new("/agents/npx/payload.js")).await;
            let record = InstalledAgentRecord {
                registry_version: "1.0.0".to_string(),
                launch: InstalledAgentLaunch::Npx {
                    package: "test-agent@1.0.0".to_string(),
                    executable: elsewhere.clone(),
                    args: Vec::new(),
                    env: HashMap::default(),
                },
                last_started: None,
            };
            write_installed_record(fs.as_ref(), &record_path, &record)
                .await
                .unwrap();
            let installs_before = npx_installs(&installs);

            let (launched, _) = resolve_npx(&fs, &installs, "1.0.0", false).await;
            assert_eq!(npx_installs(&installs), installs_before + 1, "{elsewhere}");
            assert!(launched.executable.starts_with(NPX_INSTALL_DIR));
        }

        // A `bin` that climbs into another package of the same install is
        // still a record.
        let sibling = format!("{NPX_INSTALL_DIR}/node_modules/test-agent/../other/cli.js");
        put_file(
            &fs,
            Path::new(&format!("{NPX_INSTALL_DIR}/node_modules/other/cli.js")),
        )
        .await;
        let record = InstalledAgentRecord {
            registry_version: "1.0.0".to_string(),
            launch: InstalledAgentLaunch::Npx {
                package: "test-agent@1.0.0".to_string(),
                executable: sibling.clone(),
                args: Vec::new(),
                env: HashMap::default(),
            },
            last_started: None,
        };
        write_installed_record(fs.as_ref(), &record_path, &record)
            .await
            .unwrap();
        let installs_before = npx_installs(&installs);
        let (launched, _) = resolve_npx(&fs, &installs, "1.0.0", false).await;
        assert_eq!(npx_installs(&installs), installs_before);
        assert_eq!(launched.executable, PathBuf::from(sibling));
    }

    /// ADR-F99: a binary agent's record only ever names one of the builds in
    /// the installation directory it sits in.
    #[gpui::test]
    async fn a_binary_record_naming_a_directory_elsewhere_is_no_record(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let installation_dir = Path::new("/agents/registry/test-agent");
        let record_path = installation_dir.join(INSTALLED_RECORD_FILE_NAME);
        let build =
            versioned_archive_cache_dir(installation_dir, Some("1.0.0"), TEST_ARCHIVE_URL, None);
        let record = |directory: &Path| InstalledAgentRecord {
            registry_version: "1.0.0".to_string(),
            launch: InstalledAgentLaunch::Archive {
                archive_url: TEST_ARCHIVE_URL.to_string(),
                sha256: None,
                directory: directory.to_string_lossy().into_owned(),
                cmd: "./agent".to_string(),
                args: Vec::new(),
                env: HashMap::default(),
            },
            last_started: None,
        };
        for directory in [
            build.clone(),
            PathBuf::from("/elsewhere/v_1"),
            installation_dir.join("not-a-build"),
        ] {
            put_file(&fs, &directory.join("agent")).await;
        }

        write_installed_record(fs.as_ref(), &record_path, &record(&build))
            .await
            .unwrap();
        assert!(
            usable_host_archive_record(fs.as_ref(), &record_path)
                .await
                .is_some()
        );
        for elsewhere in [
            PathBuf::from("/elsewhere/v_1"),
            installation_dir.join("not-a-build"),
        ] {
            write_installed_record(fs.as_ref(), &record_path, &record(&elsewhere))
                .await
                .unwrap();
            assert_eq!(
                usable_host_archive_record(fs.as_ref(), &record_path).await,
                None,
                "{}",
                elsewhere.display()
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn a_wsl_archive_record_only_names_this_agents_builds() {
        let own = wsl_runtime::archive_relative("test-agent", "1.0.0", TEST_ARCHIVE_URL, None);
        assert!(wsl_runtime::is_archive_relative_of("test-agent", &own));
        let other = wsl_runtime::archive_relative("other-agent", "1.0.0", TEST_ARCHIVE_URL, None);
        assert!(!wsl_runtime::is_archive_relative_of("test-agent", &other));
        let parent = own.rsplit_once('/').map(|(parent, _)| parent).unwrap();
        assert!(!wsl_runtime::is_archive_relative_of("test-agent", parent));
        assert!(!wsl_runtime::is_archive_relative_of(
            "test-agent",
            &format!("{own}/nested")
        ));
        assert!(!wsl_runtime::is_archive_relative_of(
            "test-agent",
            &format!("{parent}/not-a-build")
        ));
    }

    /// ADR-F99: a newer registry version is reported, never installed unasked;
    /// the start the user agreed to installs it and records it, once.
    #[gpui::test]
    async fn an_npx_agent_is_only_updated_once_agreed_to(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let installs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let record_path = Path::new(NPX_INSTALL_DIR).join(INSTALLED_RECORD_FILE_NAME);
        let (old, _) = resolve_npx(&fs, &installs, "1.0.0", false).await;

        let (launched, newer) = resolve_npx(&fs, &installs, "2.0.0", false).await;
        assert_eq!(npx_installs(&installs), 1, "nothing installed unasked");
        assert_eq!(launched, old, "the recorded version still launches");
        assert_eq!(launched.args, vec!["--from-1.0.0".to_string()]);
        assert_eq!(newer.as_deref(), Some("2.0.0"));
        assert_eq!(
            recorded_version(&fs, &record_path).await.as_deref(),
            Some("1.0.0")
        );

        let (updated, newer) = resolve_npx(&fs, &installs, "2.0.0", true).await;
        assert_eq!(npx_installs(&installs), 2);
        assert_eq!(newer, None);
        assert_ne!(updated.executable, old.executable);
        assert_eq!(updated.args, vec!["--from-2.0.0".to_string()]);
        assert_eq!(
            recorded_version(&fs, &record_path).await.as_deref(),
            Some("2.0.0")
        );

        let (_, newer) = resolve_npx(&fs, &installs, "2.0.0", false).await;
        assert_eq!(npx_installs(&installs), 2);
        assert_eq!(newer, None);
    }

    /// ADR-F99: the agreement for the place a start installs in is taken by
    /// that start, so the start after it is an ordinary one again; the
    /// agreement for another place stays for that place's start, and a
    /// command for a terminal sign-in takes none.
    #[gpui::test]
    async fn a_start_takes_the_agreement_for_its_place(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.executor());
        let approvals = AgentUpdateApprovals::default();
        let agent_id = AgentId::new("test-agent");
        let host = AgentInstallPlace::Host;
        let distro = AgentInstallPlace::wsl(Some("Ubuntu"));
        approvals.approve(&agent_id, host.clone());
        approvals.approve(&agent_id, distro.clone());
        let mut agent = cx.update(|cx| {
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));
            let project_environment = cx.new(|cx| {
                crate::ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx)
            });
            LocalRegistryNpxAgent {
                fs: fs.clone(),
                http_client: http_client::FakeHttpClient::with_404_response(),
                node_runtime: NodeRuntime::unavailable(),
                project_environment,
                registry_id: Arc::from("test-agent"),
                version: "2.0.0".into(),
                package: "test-agent@2.0.0".into(),
                args: Vec::new(),
                distribution_env: HashMap::default(),
                settings_env: HashMap::default(),
                update_approvals: approvals.clone(),
                new_version_available_tx: NewVersionSlot::default(),
                loading_status_tx: None,
            }
        });
        // No Node.js in a test: the install itself fails, which is beside the
        // point here.
        let sign_in = cx.update(|cx| {
            agent.get_command_without_update(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Host,
                &mut cx.to_async(),
            )
        });
        sign_in.await.ok();
        assert!(
            approvals.is_approved(&agent_id, &host),
            "a sign-in is not the agent's start"
        );

        let start = cx.update(|cx| {
            agent.get_command(
                Vec::new(),
                HashMap::default(),
                AgentLaunchTarget::Host,
                &mut cx.to_async(),
            )
        });
        start.await.ok();
        assert!(
            !approvals.is_approved(&agent_id, &host),
            "the start took the agreement"
        );
        assert!(
            approvals.is_approved(&agent_id, &distro),
            "the distro's agreement waits for a start there"
        );
    }

    #[cfg(feature = "test-support")]
    fn counting_http_client(
        body: Vec<u8>,
    ) -> (Arc<dyn HttpClient>, Arc<std::sync::atomic::AtomicUsize>) {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let client = FakeHttpClient::create({
            let requests = requests.clone();
            move |_| {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = body.clone();
                async move {
                    Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(body))?)
                }
            }
        });
        (client, requests)
    }

    #[cfg(feature = "test-support")]
    async fn start_archive_agent(
        agent: &mut LocalRegistryArchiveAgent,
        cx: &mut TestAppContext,
    ) -> AgentServerCommand {
        let command = cx
            .update(|cx| {
                agent.get_command(
                    Vec::new(),
                    HashMap::default(),
                    AgentLaunchTarget::Host,
                    &mut cx.to_async(),
                )
            })
            .await
            .expect("the agent starts");
        cx.run_until_parked();
        command
    }

    /// ADR-F99: a build that is recorded starts as it is, even where the
    /// registry now describes the same version differently (another digest,
    /// which before this would have been downloaded into a directory of its
    /// own) — and a build cached before the records existed is recorded on
    /// its first start, without a download.
    #[cfg(feature = "test-support")]
    #[gpui::test]
    async fn a_recorded_binary_agent_starts_without_a_download(cx: &mut TestAppContext) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let cached_dir =
            versioned_archive_cache_dir(&installation_dir, Some("1.0.0"), TEST_ARCHIVE_URL, None);
        std::fs::create_dir_all(&cached_dir).unwrap();
        std::fs::write(cached_dir.join("agent"), b"cached agent").unwrap();

        let (http_client, requests) = counting_http_client(b"downloaded agent".to_vec());
        let mut agent =
            make_registry_archive_agent(cx, installation_dir.clone(), http_client.clone(), None);
        let command = start_archive_agent(&mut agent, cx).await;
        assert_eq!(command.path, cached_dir.join("agent"));
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            installation_dir.join(INSTALLED_RECORD_FILE_NAME).exists(),
            "the cached build is recorded"
        );

        let republished_sha256 = format!("{:x}", Sha256::digest(b"downloaded agent"));
        let mut agent = make_registry_archive_agent(
            cx,
            installation_dir.clone(),
            http_client,
            Some(republished_sha256),
        );
        let command = start_archive_agent(&mut agent, cx).await;
        assert_eq!(command.path, cached_dir.join("agent"));
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nothing is asked of the network while the record stands"
        );
    }

    /// ADR-F99: a newer version in the registry is reported at start, and the
    /// recorded build keeps starting until the user agrees; the start they
    /// agreed to downloads the new one, records it and takes the agreement.
    #[cfg(feature = "test-support")]
    #[gpui::test]
    async fn a_binary_agent_is_only_updated_once_agreed_to(cx: &mut TestAppContext) {
        init_test_settings(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let installation_dir = temp_dir.path().join("agent");
        let record_path = installation_dir.join(INSTALLED_RECORD_FILE_NAME);
        let (http_client, requests) = counting_http_client(b"agent".to_vec());
        let mut agent =
            make_registry_archive_agent(cx, installation_dir.clone(), http_client.clone(), None);
        let old = start_archive_agent(&mut agent, cx).await;
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut agent =
            make_registry_archive_agent(cx, installation_dir.clone(), http_client.clone(), None);
        agent.version = "2.0.0".into();
        let (tx, mut rx) = watch::channel::<Option<String>>(None);
        agent.set_new_version_available_tx(tx);
        let launched = start_archive_agent(&mut agent, cx).await;
        assert_eq!(launched.path, old.path, "the recorded build still starts");
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no download unasked"
        );
        assert_eq!(
            rx.borrow().as_deref(),
            Some("2.0.0"),
            "the start reports it"
        );

        agent
            .update_approvals
            .approve(&AgentId::new("test-agent"), AgentInstallPlace::Host);
        let updated = start_archive_agent(&mut agent, cx).await;
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            updated.path,
            versioned_archive_cache_dir(&installation_dir, Some("2.0.0"), TEST_ARCHIVE_URL, None)
                .join("agent")
        );
        assert!(
            !agent
                .update_approvals
                .is_approved(&AgentId::new("test-agent"), &AgentInstallPlace::Host)
        );
        let fs = fs::RealFs::new(None, cx.executor());
        assert_eq!(
            read_installed_record(fs.as_ref(), &record_path)
                .await
                .map(|record| record.registry_version)
                .as_deref(),
            Some("2.0.0")
        );

        let again = start_archive_agent(&mut agent, cx).await;
        assert_eq!(again.path, updated.path);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}

/// ADR-F49 決定 19: an agent this host could not put in its sandbox is
/// described *with the reason*, so the client refuses to start it and offers
/// to start it outside the sandbox once — rather than quietly starting it
/// unsandboxed.
fn sandbox_placement_failed(
    mut sandbox: AgentSandboxCommand,
    reason: Option<String>,
) -> AgentSandboxCommand {
    if let Some(reason) = reason {
        sandbox.state = AgentSandboxState::Unavailable { reason };
    }
    sandbox
}

/// ADR-F49 第 2b 段: the command for an npx-distributed registry agent as it
/// runs **inside** the project's WSL distro — Zed's own Linux Node.js staged
/// there, the package installed there by a sandboxed npm, and both shown back
/// to the agent read-only.
#[cfg(target_os = "windows")]
#[allow(
    clippy::too_many_arguments,
    reason = "one registry entry's whole description"
)]
async fn npx_agent_command_in_wsl(
    wsl_distro: Option<String>,
    executor: gpui::BackgroundExecutor,
    http_client: Arc<dyn HttpClient>,
    fs: Arc<dyn Fs>,
    registry_id: &str,
    registry_version: &str,
    package: &str,
    args: Vec<String>,
    extra_args: Vec<String>,
    distribution_env: HashMap<String, String>,
    extra_env: HashMap<String, String>,
    settings_env: HashMap<String, String>,
    editor_network_allowlist: Option<Vec<String>>,
    update_approved: bool,
    new_version_available: &NewVersionSlot,
    loading_status_tx: &mut Option<watch::Sender<Option<String>>>,
) -> Result<AgentServerCommand> {
    let distro = wsl_distro.as_deref();
    let node_directory =
        wsl_runtime::node_directory(distro, &executor, &http_client, &fs, loading_status_tx)
            .await?;
    let package = wsl_runtime::npm_package(
        distro,
        &executor,
        &fs,
        &node_directory,
        registry_id,
        registry_version,
        package,
        args,
        distribution_env,
        update_approved,
        editor_network_allowlist,
        loading_status_tx,
    )
    .await?;
    if let Some(newer) = package.newer {
        report_new_version(new_version_available, newer);
    }
    let wsl_runtime::WslNpmPackage {
        install_directory,
        executable,
        args,
        env: distribution_env,
        ..
    } = package;
    let agent_env = agent_environment(distribution_env, extra_env, settings_env);

    // The agent's environment is the one written for the distro: the staged
    // Node.js first on `PATH`, then what the entry and the user's settings
    // ask for. Nothing of this Windows process's environment goes in.
    let mut env = HashMap::default();
    env.insert("PATH".to_string(), wsl_runtime::linux_path(&node_directory));
    env.extend(agent_env);

    let mut command_args = vec![executable];
    command_args.extend(args);
    command_args.extend(extra_args);

    Ok(AgentServerCommand {
        sandbox: Some(AgentSandboxCommand {
            env,
            readable_paths: vec![
                PathBuf::from(node_directory.clone()),
                PathBuf::from(install_directory),
            ],
            state: AgentSandboxState::Unresolved {
                profile: SandboxedAgentProfile::for_agent(registry_id, true),
            },
            wsl: Some(WslAgentRuntime { distro: wsl_distro }),
        }),
        path: PathBuf::from(format!("{node_directory}/bin/node")),
        args: command_args,
        // A Linux command takes no Windows environment: everything it needs is
        // in `sandbox.env`, which the WSL helper sets inside the sandbox.
        env: None,
    })
}

/// ADR-F49 第 2b 段: the command for a registry agent distributed as an
/// archive, as it runs **inside** the project's WSL distro — the entry's
/// **Linux** build staged there rather than this machine's Windows one.
#[cfg(target_os = "windows")]
#[allow(
    clippy::too_many_arguments,
    reason = "one registry entry's whole description"
)]
async fn registry_archive_command_in_wsl(
    wsl_distro: Option<String>,
    executor: gpui::BackgroundExecutor,
    http_client: Arc<dyn HttpClient>,
    fs: Arc<dyn Fs>,
    registry_id: &str,
    version: &str,
    targets: &HashMap<String, RegistryTargetConfig>,
    extra_env: HashMap<String, String>,
    settings_env: HashMap<String, String>,
    extra_args: Vec<String>,
    update_approved: bool,
    new_version_available: &NewVersionSlot,
    loading_status_tx: &mut Option<watch::Sender<Option<String>>>,
) -> Result<AgentServerCommand> {
    let distro = wsl_distro.as_deref();

    // ADR-F99: the build this distro has is launched as it is — no digest is
    // asked of GitHub and nothing is downloaded — unless the user agreed to
    // the registry's newer version.
    let record_path = crate::agent_update::wsl_record_path(distro, registry_id);
    let recorded = match read_installed_record(fs.as_ref(), &record_path).await {
        Some(record) => match record.launch.clone() {
            // The runtime directory becomes readable inside the sandbox, so a
            // record (a file on this host) only ever names one of this agent's
            // own builds.
            InstalledAgentLaunch::Archive {
                sha256,
                directory,
                cmd,
                args,
                env,
                ..
            } if wsl_runtime::is_archive_relative_of(registry_id, &directory) => {
                wsl_runtime::bounded(
                    &executor,
                    wsl_runtime::STEP_TIMEOUT,
                    i18n::tc!("wsl sandbox step", "Checking the agent's runtime in WSL"),
                    sandbox::staged_wsl_runtime(distro, &directory, sha256.as_deref()),
                )
                .await?
                .map(|runtime_directory| (record, runtime_directory, cmd, args, env))
            }
            _ => None,
        },
        None => None,
    };
    let decision = launch_decision(
        recorded
            .as_ref()
            .map(|(record, ..)| record.registry_version.as_str()),
        version,
        update_approved,
    );
    let (runtime_directory, cmd, target_args, target_env) = match (decision, recorded) {
        (LaunchDecision::Recorded { newer }, Some((record, runtime_directory, cmd, args, env))) => {
            if let Some(newer) = newer {
                report_new_version(new_version_available, newer);
            }
            record_start(fs.as_ref(), &record_path, record).await;
            (runtime_directory, cmd, args, env)
        }
        _ => {
            let machine = wsl_runtime::machine_architecture(distro, &executor).await?;
            let architecture = match machine.as_str() {
                "x86_64" | "amd64" => "x86_64",
                "aarch64" | "arm64" => "aarch64",
                other => bail!(i18n::t!(
                    "Distributions for the WSL architecture `{other}` are not supported, so the agent \
                     cannot start inside the sandbox."
                )),
            };
            let platform_key = format!("linux-{architecture}");
            let target_config = targets.get(&platform_key).with_context(|| {
                i18n::t!(
                    "This agent has no distribution for {platform_key}, so it cannot start inside the \
                     WSL sandbox (available: {available}).",
                    available = targets
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(i18n::tc!("list separator", ", "))
                )
            })?;

            let sha256 = registry_archive_sha256(
                &http_client,
                &target_config.archive,
                target_config.sha256.clone(),
            )
            .await;
            let relative = wsl_runtime::archive_relative(
                registry_id,
                version,
                &target_config.archive,
                sha256.as_deref(),
            );
            let runtime_directory = wsl_runtime::registry_archive(
                distro,
                &executor,
                &http_client,
                &fs,
                registry_id,
                version,
                &target_config.archive,
                sha256.clone(),
                loading_status_tx,
            )
            .await?;
            let record = InstalledAgentRecord::started_now(
                version.to_string(),
                InstalledAgentLaunch::Archive {
                    archive_url: target_config.archive.clone(),
                    sha256,
                    directory: relative,
                    cmd: target_config.cmd.clone(),
                    args: target_config.args.clone(),
                    env: target_config.env.clone(),
                },
            );
            write_installed_record(fs.as_ref(), &record_path, &record)
                .await
                .log_err();
            (
                runtime_directory,
                target_config.cmd.clone(),
                target_config.args.clone(),
                target_config.env.clone(),
            )
        }
    };

    let mut readable_paths = vec![PathBuf::from(runtime_directory.clone())];
    let mut env = HashMap::default();
    let program = if cmd == "node" {
        let node_directory =
            wsl_runtime::node_directory(distro, &executor, &http_client, &fs, loading_status_tx)
                .await?;
        env.insert("PATH".to_string(), wsl_runtime::linux_path(&node_directory));
        let program = format!("{node_directory}/bin/node");
        readable_paths.push(PathBuf::from(node_directory));
        program
    } else {
        let relative = cmd
            .strip_prefix("./")
            .or_else(|| cmd.strip_prefix(".\\"))
            .with_context(|| {
                i18n::t!(
                    "The agent's executable `{command}` has to be a relative path starting with `./`.",
                    command = cmd
                )
            })?;
        wsl_runtime::bounded(
            &executor,
            wsl_runtime::STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the agent's executable"),
            sandbox::wsl_runtime_executable(distro, &runtime_directory, relative),
        )
        .await?
    };

    env.extend(agent_environment(target_env, extra_env, settings_env));

    let mut args = target_args;
    args.extend(extra_args);

    Ok(AgentServerCommand {
        sandbox: Some(AgentSandboxCommand {
            env,
            readable_paths,
            state: AgentSandboxState::Unresolved {
                profile: SandboxedAgentProfile::for_agent(registry_id, true),
            },
            wsl: Some(WslAgentRuntime { distro: wsl_distro }),
        }),
        path: PathBuf::from(program),
        args,
        env: None,
    })
}

/// ADR-F49 第 2b 段: resolving an agent's command for the WSL distro a
/// Windows project's folders live in.
///
/// A Windows agent runs *inside* that distro (決定 20), so everything it needs
/// — the Linux Node.js, the npm package or the Linux build of a registry entry
/// — is obtained here under the editor's own egress control, checked, and
/// staged into the distro's Zed data directory, where the sandbox helper
/// protects it and may show it back read-only (決定 8 / 11 / 14). Nothing is
/// unpacked on Windows (決定 20) and nothing of a package is run outside the
/// sandbox (決定 22).
#[cfg(target_os = "windows")]
mod wsl_runtime {
    use std::future::Future;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{Context as _, Result, anyhow, bail};
    use collections::HashMap;
    use fs::{Fs, RemoveOptions};
    use gpui::{App, BackgroundExecutor, FutureExt as _};
    use http_client::{HttpClient, github::AssetKind};
    use semver::Version;
    use util::ResultExt as _;

    use crate::agent_update::{
        InstalledAgentLaunch, InstalledAgentRecord, read_installed_record, write_installed_record,
    };

    use super::{
        RegistryArchiveKind, archive_version_component, registry_archive_kind_for_url,
        sanitize_path_component,
    };

    /// Where the Linux runtimes live below the distro's Zed data directory.
    /// `wsl-sandbox` keeps them apart from what a **WSL remote** connection
    /// installs in the same data directory (its `remote_server` resolves
    /// agents for itself, in `external_agents/registry/...`), so the two never
    /// overwrite each other.
    const NODE_RELATIVE: &str = "node/wsl-sandbox";
    const NPX_RELATIVE: &str = "external_agents/wsl-sandbox/registry/npx";
    const ARCHIVE_RELATIVE: &str = "external_agents/wsl-sandbox/registry/archive";

    /// The registry an npm package is installed from, and the only host the
    /// sandboxed npm may reach besides what the editor-wide allowlist permits.
    const NPM_REGISTRY_HOST: &str = "registry.npmjs.org";

    /// Where a runtime is downloaded to before it is copied into the distro.
    /// Deliberately **not** below `paths::data_dir()/node`: this host's own
    /// `ManagedNodeRuntime` empties that whole directory when it reinstalls
    /// Node.js for itself, which would take a download in progress with it.
    /// Each file is removed as soon as it has been staged.
    pub(super) fn downloads_directory() -> PathBuf {
        paths::external_agents_dir().join("downloads")
    }

    /// The `PATH` an agent started in the distro gets: the staged Node.js
    /// first, then the system's own directories.
    pub(super) fn linux_path(node_directory: &str) -> String {
        format!("{node_directory}/bin:/usr/local/bin:/usr/bin:/bin")
    }

    /// The editor-wide network allowlist as it stands now, which the WSL
    /// helper's proxy nests an install's own hosts inside (ADR-F17 / 決定 13).
    /// `None` in a process with no editor-wide allowlist, where the sandbox
    /// has nothing to nest inside.
    pub(super) fn editor_network_allowlist(cx: &App) -> Option<Vec<String>> {
        let global = cx.try_global::<egress_policy::EgressAllowlistGlobal>()?;
        Some(
            global
                .0
                .snapshot()
                .patterns()
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    }

    fn report(status: &mut Option<watch::Sender<Option<String>>>, message: &str) {
        if let Some(status) = status.as_mut() {
            status.send(Some(message.to_string())).ok();
        }
    }

    /// One `wsl.exe` round trip, which a cold WSL can take this long to answer
    /// (the same bound the interactive Claude session uses).
    pub(super) const STEP_TIMEOUT: Duration = Duration::from_secs(90);
    /// Copying a runtime into the distro, unpacking it there, or running a
    /// whole `npm install` inside the sandbox — minutes of real work on a slow
    /// link, but not forever.
    const WORK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    /// Bounds one step inside WSL. The `sandbox` crate deliberately keeps no
    /// timer of its own, and a wedged `wsl.exe` (a real failure mode when the
    /// WSL service is unhealthy) would otherwise leave the agent starting for
    /// ever; dropping the future here kills the `wsl.exe` it left running.
    pub(super) async fn bounded<T>(
        executor: &BackgroundExecutor,
        timeout: Duration,
        what: &str,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        work.with_timeout(timeout, executor).await.map_err(|_| {
            anyhow!(i18n::t!(
                "{what} did not finish within {seconds} seconds. Wait a while and try again, and \
                 if that does not help, restart WSL with `wsl --shutdown`.",
                seconds = timeout.as_secs()
            ))
        })?
    }

    /// ADR-F49 決定 20 / 21: the Linux Node.js staged in `distro`, obtaining it
    /// from nodejs.org against the digest this build pins if it is not there
    /// yet. Returns its directory inside the distro.
    pub(super) async fn node_directory(
        distro: Option<&str>,
        executor: &BackgroundExecutor,
        http_client: &Arc<dyn HttpClient>,
        fs: &Arc<dyn Fs>,
        status: &mut Option<watch::Sender<Option<String>>>,
    ) -> Result<String> {
        let machine = machine_architecture(distro, executor).await?;
        let architecture = match machine.as_str() {
            "x86_64" | "amd64" => "x64",
            "aarch64" | "arm64" => "arm64",
            other => bail!(i18n::t!(
                "Node.js is not distributed for the WSL architecture `{other}`, so the agent \
                 cannot start inside the sandbox."
            )),
        };
        let archive =
            node_runtime::pinned_node_archive("linux", architecture).with_context(|| {
                i18n::t!("There is no download information for Node.js for Linux ({architecture}).")
            })?;
        anyhow::ensure!(
            !archive.is_zip,
            i18n::t!("Node.js for Linux is a zip archive (unexpected).")
        );
        let relative = format!("{NODE_RELATIVE}/{}", archive.directory_name());
        if let Some(staged) = bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking Node.js in WSL"),
            sandbox::staged_wsl_runtime(distro, &relative, Some(archive.sha256)),
        )
        .await?
        {
            return Ok(staged);
        }

        report(status, i18n::t!("Downloading Node.js for Linux…"));
        let downloads = downloads_directory();
        fs.create_dir(&downloads)
            .await
            .with_context(|| format!("creating {}", downloads.display()))?;
        let local = downloads.join(&archive.file_name);
        node_runtime::download_verified_file(
            http_client.as_ref(),
            &archive.url,
            Some(archive.sha256),
            &local,
        )
        .await
        .with_context(|| {
            i18n::t!(
                "Could not download Node.js for Linux ({url}). Check that the editor's allowlist \
                 of connections includes `nodejs.org`.",
                url = archive.url
            )
        })?;

        report(status, i18n::t!("Placing Node.js for Linux in WSL…"));
        let staged = bounded(
            executor,
            WORK_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Placing Node.js for Linux in WSL"),
            sandbox::stage_wsl_runtime_archive(
                distro,
                &relative,
                &local,
                archive.sha256,
                sandbox::WslArchiveKind::TarGz,
                // A Node.js tarball wraps everything in one
                // `node-<version>-linux-<arch>/` directory.
                true,
            ),
        )
        .await;
        remove_download(fs, &local).await;
        staged
    }

    /// ADR-F99: an npm package in a distro, as a start launches it.
    pub(super) struct WslNpmPackage {
        pub(super) install_directory: String,
        pub(super) executable: String,
        pub(super) args: Vec<String>,
        pub(super) env: HashMap<String, String>,
        /// The registry's version, when it is newer than the one launched and
        /// the user has not agreed to it yet.
        pub(super) newer: Option<String>,
    }

    /// ADR-F49 決定 22: the npm package `package` installed in `distro` for
    /// the registry entry `registry_id`, installed (inside the sandbox) when
    /// there is none yet. ADR-F99: once this host has a record of what it
    /// installed there, that is launched as it is until the user agrees to the
    /// registry's newer version ([`crate::agent_update::wsl_npm_plan`]).
    /// Returns its install directory, the executable to run with Node.js, and
    /// the arguments and environment the install was made with.
    #[allow(
        clippy::too_many_arguments,
        reason = "one registry entry's npm install, end to end"
    )]
    pub(super) async fn npm_package(
        distro: Option<&str>,
        executor: &BackgroundExecutor,
        fs: &Arc<dyn Fs>,
        node_directory: &str,
        registry_id: &str,
        registry_version: &str,
        package: &str,
        args: Vec<String>,
        distribution_env: HashMap<String, String>,
        update_approved: bool,
        editor_network_allowlist: Option<Vec<String>>,
        status: &mut Option<watch::Sender<Option<String>>>,
    ) -> Result<WslNpmPackage> {
        let relative = format!("{NPX_RELATIVE}/{}", sanitize_path_component(registry_id));
        let data_directory = bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the data folder in WSL"),
            sandbox::wsl_agent_runtime_data_directory(distro),
        )
        .await?;
        let install_directory = format!("{data_directory}/{relative}");

        let record_path = crate::agent_update::wsl_record_path(distro, registry_id);
        let recorded = match read_installed_record(fs.as_ref(), &record_path).await {
            Some(InstalledAgentRecord {
                registry_version,
                launch:
                    InstalledAgentLaunch::Npx {
                        package, args, env, ..
                    },
                ..
            }) => Some((registry_version, package, args, env)),
            _ => None,
        };
        // What is installed is read under the name the record gives it, which
        // is the package launched while the record stands.
        let installed_package = recorded
            .as_ref()
            .map(|(_, package, ..)| package.as_str())
            .unwrap_or(package);
        let (installed_name, _) = super::bounded_npm_package_spec(installed_package);
        let installed =
            installed_npm_version(distro, executor, &install_directory, installed_name).await;
        let plan = crate::agent_update::wsl_npm_plan(
            recorded
                .as_ref()
                .map(|(registry_version, ..)| registry_version.as_str()),
            installed.as_ref(),
            pinned_package_version(package).as_ref(),
            registry_version,
            update_approved,
        );
        let keeps_record = !plan.install && recorded.is_some();
        let (recorded_for, package, args, distribution_env) = match recorded {
            Some((recorded_version, recorded_package, recorded_args, recorded_env))
                if keeps_record =>
            {
                (
                    recorded_version,
                    recorded_package,
                    recorded_args,
                    recorded_env,
                )
            }
            _ => (
                registry_version.to_string(),
                package.to_string(),
                args,
                distribution_env,
            ),
        };
        let (package_name, package_spec) = super::bounded_npm_package_spec(&package);
        if plan.install {
            report(status, i18n::t!("Running npm install inside the sandbox…"));
            bounded(
                executor,
                WORK_TIMEOUT,
                i18n::tc!("wsl sandbox step", "npm install inside the sandbox"),
                sandbox::install_npm_package_in_wsl(
                    distro,
                    node_directory,
                    &package_spec,
                    &relative,
                    vec![NPM_REGISTRY_HOST.to_string()],
                    editor_network_allowlist,
                ),
            )
            .await
            .with_context(|| {
                i18n::t!(
                    "Could not install `{package_spec}` inside the WSL sandbox. Check that the \
                     editor's allowlist of connections includes `{NPM_REGISTRY_HOST}`."
                )
            })?;
        }

        let package_json = bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the npm package"),
            sandbox::read_wsl_npm_package_json(distro, &install_directory, package_name),
        )
        .await?
        .with_context(|| i18n::t!("`{package_name}` is not installed in WSL."))?;
        let executable_relative =
            node_runtime::package_json_executable(&package_json, package_name)?;
        let executable = bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the npm package's executable"),
            sandbox::wsl_npm_package_executable(
                distro,
                &install_directory,
                package_name,
                &executable_relative,
            ),
        )
        .await?;

        // Written by every start, the one that keeps the record too, for the
        // time it says this distro last started the agent.
        let record = InstalledAgentRecord::started_now(
            recorded_for,
            InstalledAgentLaunch::Npx {
                package: package.clone(),
                executable: executable.clone(),
                args: args.clone(),
                env: distribution_env.clone(),
            },
        );
        write_installed_record(fs.as_ref(), &record_path, &record)
            .await
            .log_err();
        Ok(WslNpmPackage {
            install_directory,
            executable,
            args,
            env: distribution_env,
            newer: plan.newer,
        })
    }

    /// The version of `package_name` installed in the distro, or `None` when
    /// nothing complete is installed (which is also how a `package.json` that
    /// cannot be read is treated: it is installed again).
    async fn installed_npm_version(
        distro: Option<&str>,
        executor: &BackgroundExecutor,
        install_directory: &str,
        package_name: &str,
    ) -> Option<Version> {
        bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the installed npm package"),
            sandbox::read_wsl_npm_package_json(distro, install_directory, package_name),
        )
        .await
        .log_err()
        .flatten()
        .and_then(|package_json| node_runtime::package_json_version(&package_json).log_err())
    }

    /// The distro's machine architecture, bounded like every other step.
    pub(super) async fn machine_architecture(
        distro: Option<&str>,
        executor: &BackgroundExecutor,
    ) -> Result<String> {
        bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the WSL architecture"),
            sandbox::wsl_machine_architecture(distro),
        )
        .await
    }

    /// The version a registry entry's package specification pins, when it
    /// pins one (`name@1.2.3`).
    fn pinned_package_version(package: &str) -> Option<Version> {
        let (name, version) = package.rsplit_once('@')?;
        if name.is_empty() {
            return None;
        }
        Version::parse(version).ok()
    }

    /// ADR-F49 決定 20 / 25: the Linux build of a registry entry, staged in
    /// `distro`. Returns the directory it was staged in, which the entry's
    /// `cmd` is resolved against and which the sandbox shows the agent
    /// read-only.
    #[allow(
        clippy::too_many_arguments,
        reason = "one registry target's whole description"
    )]
    pub(super) async fn registry_archive(
        distro: Option<&str>,
        executor: &BackgroundExecutor,
        http_client: &Arc<dyn HttpClient>,
        fs: &Arc<dyn Fs>,
        registry_id: &str,
        version: &str,
        archive_url: &str,
        sha256: Option<String>,
        status: &mut Option<watch::Sender<Option<String>>>,
    ) -> Result<String> {
        let kind = registry_archive_kind_for_url(archive_url)?;
        let (archive_kind, file_name) = match &kind {
            RegistryArchiveKind::Archive(AssetKind::TarGz) => {
                (sandbox::WslArchiveKind::TarGz, None)
            }
            RegistryArchiveKind::Archive(AssetKind::TarBz2) => {
                (sandbox::WslArchiveKind::TarBz2, None)
            }
            RegistryArchiveKind::Archive(AssetKind::Gz) => bail!(i18n::t!(
                "This distribution's format cannot be unpacked in the WSL sandbox. Choose an \
                 agent with a tar.gz distribution for Linux, or start it outside the sandbox."
            )),
            RegistryArchiveKind::Archive(AssetKind::Zip) => bail!(i18n::t!(
                "This distribution is a zip archive, so it cannot be unpacked in the WSL sandbox \
                 (the WSL distribution has no `unzip`). Choose an agent with a tar.gz distribution \
                 for Linux, or start it outside the sandbox."
            )),
            RegistryArchiveKind::RawBinary { file_name } => {
                (sandbox::WslArchiveKind::TarGz, Some(file_name.clone()))
            }
        };
        let relative = archive_relative(registry_id, version, archive_url, sha256.as_deref());
        // A build already staged is reused: under the digest the registry
        // names where there is one, and otherwise simply for being complete
        // where this version at this URL puts it — the same identity the local
        // installation is kept under (`versioned_archive_cache_dir`), and all
        // there is to go on without a digest (ADR-F49 決定 25).
        if let Some(directory) = bounded(
            executor,
            STEP_TIMEOUT,
            i18n::tc!("wsl sandbox step", "Checking the agent's runtime in WSL"),
            sandbox::staged_wsl_runtime(distro, &relative, sha256.as_deref()),
        )
        .await?
        {
            return Ok(directory);
        }

        report(status, i18n::t!("Downloading the agent for Linux…"));
        let downloads = downloads_directory();
        fs.create_dir(&downloads)
            .await
            .with_context(|| format!("creating {}", downloads.display()))?;
        let local = downloads.join(format!(
            "agent-{}-{}",
            sanitize_path_component(registry_id),
            sanitize_path_component(version)
        ));
        let digest = node_runtime::download_verified_file(
            http_client.as_ref(),
            archive_url,
            sha256.as_deref(),
            &local,
        )
        .await
        .with_context(|| i18n::t!("Could not download {archive_url}."))?;

        report(status, i18n::t!("Placing the agent for Linux in WSL…"));
        let staged = match &file_name {
            // ADR-F49 決定 25: published as a plain executable rather than an
            // archive. Nothing vouches for it beyond HTTPS when the registry
            // names no digest; the digest of what was downloaded still keeps
            // the copy into the distro honest.
            Some(name) => bounded(
                executor,
                WORK_TIMEOUT,
                i18n::tc!("wsl sandbox step", "Placing the agent for Linux in WSL"),
                sandbox::stage_wsl_runtime_file(distro, &relative, name, &local, &digest, true),
            )
            .await
            .map(|executable| {
                executable
                    .rsplit_once('/')
                    .map(|(directory, _)| directory.to_string())
                    .unwrap_or(executable)
            }),
            None => {
                bounded(
                    executor,
                    WORK_TIMEOUT,
                    i18n::tc!("wsl sandbox step", "Placing the agent for Linux in WSL"),
                    sandbox::stage_wsl_runtime_archive(
                        distro,
                        &relative,
                        &local,
                        &digest,
                        archive_kind,
                        false,
                    ),
                )
                .await
            }
        };
        remove_download(fs, &local).await;
        staged
    }

    /// Where one build of a registry archive is staged, below the distro's
    /// data directory.
    pub(super) fn archive_relative(
        registry_id: &str,
        version: &str,
        archive_url: &str,
        sha256: Option<&str>,
    ) -> String {
        format!(
            "{ARCHIVE_RELATIVE}/{}/{}",
            sanitize_path_component(registry_id),
            archive_version_component(version, archive_url, sha256)
        )
    }

    /// Whether `directory` is one of the places [`archive_relative`] stages a
    /// build of `registry_id` at.
    pub(super) fn is_archive_relative_of(registry_id: &str, directory: &str) -> bool {
        let prefix = format!(
            "{ARCHIVE_RELATIVE}/{}/",
            sanitize_path_component(registry_id)
        );
        directory.strip_prefix(&prefix).is_some_and(|build| {
            build.starts_with(super::VERSIONED_ARCHIVE_CACHE_DIR_PREFIX)
                && !build.contains(['/', '\\'])
        })
    }

    async fn remove_download(fs: &Arc<dyn Fs>, path: &PathBuf) {
        fs.remove_file(
            path,
            RemoveOptions {
                recursive: false,
                ignore_if_not_exists: true,
            },
        )
        .await
        .log_err();
    }
}
