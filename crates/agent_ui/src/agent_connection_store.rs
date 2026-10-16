use std::rc::Rc;
use std::time::{Duration, Instant};

use acp_thread::{AcpThreadEvent, AgentConnection, LoadError};
use agent_servers::AcpConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use agent_settings::AgentSettings;
use anyhow::Result;
use collections::{HashMap, HashSet};
use futures::{FutureExt, StreamExt as _, channel::mpsc, future::Shared};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EntityId, EventEmitter, SharedString, Subscription,
    Task, WeakEntity,
};
use util::ResultExt as _;

use project::agent_update::AgentInstallPlace;
use project::{AgentLaunchTarget, AgentServerStore, AgentServersUpdated, Project};
use settings::{Settings as _, SettingsStore};
use watch::Receiver;

use crate::Agent;
use crate::conversation_view::ConversationView;
use crate::thread_metadata_store::SessionOptionsSnapshot;

/// ADR-F49 (new decision): how often the store asks whether its connections
/// are still used. A connection goes at most this long after its grace ran
/// out.
///
/// Closing a thread's tab is not an event this can subscribe to — the thread
/// lives on in the panel's parking slots with its connection in hand — so the
/// question is asked from here rather than answered from there.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// ADR-F103: how long the session that reads an agent's options waits for its
/// slash commands, which agents announce after the session itself.
const SESSION_OPTIONS_COMMANDS_WAIT: Duration = Duration::from_secs(3);

/// ADR-F103: where reading what an agent's sessions offer, before the agent's
/// first thread has started, has come in this store (that is, in this panel):
/// one read at a time per agent, and none again after one failed.
#[derive(Clone)]
pub enum SessionOptionsRead {
    Reading(Shared<Task<Result<(), SharedString>>>),
    Failed,
}

pub enum AgentConnectionEntry {
    Connecting {
        connect_task: Shared<Task<Result<AgentConnectedState, LoadError>>>,
    },
    Connected(AgentConnectedState),
    Error {
        error: LoadError,
    },
}

#[derive(Clone)]
pub struct AgentConnectedState {
    pub connection: Rc<dyn AgentConnection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
}

impl AgentConnectionEntry {
    pub fn wait_for_connection(&self) -> Shared<Task<Result<AgentConnectedState, LoadError>>> {
        match self {
            AgentConnectionEntry::Connecting { connect_task } => connect_task.clone(),
            AgentConnectionEntry::Connected(state) => Task::ready(Ok(state.clone())).shared(),
            AgentConnectionEntry::Error { error } => Task::ready(Err(error.clone())).shared(),
        }
    }

    pub fn status(&self) -> AgentConnectionStatus {
        match self {
            AgentConnectionEntry::Connecting { .. } => AgentConnectionStatus::Connecting,
            AgentConnectionEntry::Connected(_) => AgentConnectionStatus::Connected,
            AgentConnectionEntry::Error { .. } => AgentConnectionStatus::Disconnected,
        }
    }
}

pub enum AgentConnectionEntryEvent {
    NewVersionAvailable(SharedString),
    LoadingStatusChanged(Option<SharedString>),
}

impl EventEmitter<AgentConnectionEntryEvent> for AgentConnectionEntry {}

#[derive(Clone)]
pub struct ActiveAcpConnection {
    pub agent_id: project::AgentId,
    pub connection: Rc<AcpConnection>,
}

/// ADR-F49 decision 19: which view a connection started outside its sandbox
/// belongs to. The user confirms such a start for one view and one time, so
/// the connection is kept apart from the shared ones and handed back to that
/// view alone; every other view and every new thread asks for a connection in
/// the sandbox as usual.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OutsideSandboxConnectionId(u64);

/// ADR-F96 stage 3: a connection whose sandbox lets the agent reach any host
/// the editor-wide allowlist permits. The user asks for it thread by thread, so
/// it is handed only to the views whose thread asked for it (or whose
/// project's default did), never to the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UnrestrictedNetworkConnectionId(u64);

/// ADR-F49 (new decision): keeps an agent's connection while something that is
/// not a view is using it. See [`AgentConnectionStore::lease_connection`].
///
/// Nothing reads what is inside: the store counts the hands on it, and holding
/// one of these is the whole of what a caller has to do.
pub struct ConnectionLease(#[allow(dead_code)] Rc<()>);

#[derive(Clone, PartialEq, Eq, Hash)]
enum ConnectionSlot {
    Shared(Agent),
    OutsideSandbox {
        id: OutsideSandboxConnectionId,
        agent: Agent,
    },
    /// ADR-F49 decision 19 (8c): the agent in its sandbox while the user's
    /// settings start it outside every time, for the threads whose session
    /// was started in the sandbox (the shared connection is the one outside).
    InSandbox(Agent),
    /// ADR-F96 stage 3: the agent in its sandbox without its own hosts.
    UnrestrictedNetwork {
        id: UnrestrictedNetworkConnectionId,
        agent: Agent,
    },
}

/// ADR-F49 decision 19 (8c): where a view's start goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionPlace {
    /// Where the user's settings put every start of the agent.
    AsTheSettingsSay,
    /// Outside the sandbox, the start the user confirmed for this view.
    OutsideSandbox(OutsideSandboxConnectionId),
    /// In the sandbox, whatever the settings say.
    InSandbox,
    /// In the sandbox without the agent's own hosts, the start the user asked
    /// for in this view (ADR-F96 stage 3).
    UnrestrictedNetwork(UnrestrictedNetworkConnectionId),
}

impl ConnectionSlot {
    fn at(agent: Agent, place: ConnectionPlace) -> Self {
        match place {
            ConnectionPlace::AsTheSettingsSay => ConnectionSlot::Shared(agent),
            ConnectionPlace::InSandbox => ConnectionSlot::InSandbox(agent),
            ConnectionPlace::OutsideSandbox(id) => ConnectionSlot::OutsideSandbox { id, agent },
            ConnectionPlace::UnrestrictedNetwork(id) => {
                ConnectionSlot::UnrestrictedNetwork { id, agent }
            }
        }
    }

    fn agent(&self) -> &Agent {
        match self {
            ConnectionSlot::Shared(agent) | ConnectionSlot::InSandbox(agent) => agent,
            ConnectionSlot::OutsideSandbox { agent, .. }
            | ConnectionSlot::UnrestrictedNetwork { agent, .. } => agent,
        }
    }

    /// ADR-F49 (new decision): whether this connection is one the idle
    /// disconnect is for. Zed's own agent runs in this process — dropping it
    /// stops nothing and only makes the panel connect again — so only the
    /// external agents, which bring a sandbox and processes of their own, are
    /// let go of.
    fn can_be_dropped_when_idle(&self) -> bool {
        !matches!(self.agent(), Agent::NativeAgent)
    }
}

pub struct AgentConnectionStore {
    project: Entity<Project>,
    entries: HashMap<Agent, Entity<AgentConnectionEntry>>,
    /// Connections started outside their sandbox, by the view that confirmed
    /// each of them.
    outside_sandbox: HashMap<OutsideSandboxConnectionId, (Agent, Entity<AgentConnectionEntry>)>,
    /// ADR-F49 decision 19 (8c): connections outside the sandbox that a newer
    /// version of their agent has superseded. They stay in `outside_sandbox`
    /// — the idle disconnect only ever looks there, and a connection it
    /// cannot see is one whose process nothing ever ends — but no start is
    /// handed one of them: the view that asks starts the new version instead.
    superseded_outside_sandbox: HashSet<OutsideSandboxConnectionId>,
    next_outside_sandbox_id: u64,
    /// ADR-F49 decision 19 (8c): see [`ConnectionSlot::InSandbox`].
    in_sandbox: HashMap<Agent, Entity<AgentConnectionEntry>>,
    /// ADR-F96 stage 3: connections without the agent's own hosts. The views
    /// that asked for one share the agent's latest
    /// ([`Self::connect_with_unrestricted_network`]).
    unrestricted_network:
        HashMap<UnrestrictedNetworkConnectionId, (Agent, Entity<AgentConnectionEntry>)>,
    /// The same as `superseded_outside_sandbox`, for `unrestricted_network`.
    superseded_unrestricted_network: HashSet<UnrestrictedNetworkConnectionId>,
    /// Versions reported by [`AgentConnectionEntryEvent::NewVersionAvailable`],
    /// kept after the event has been delivered. The event fires once and drops
    /// the connection entry with it, so without this the agent registry page
    /// has nothing to render a "update available" badge from (F2-5 follow-up).
    new_versions: HashMap<Agent, SharedString>,
    /// ADR-F49 (new decision): the views that asked for a connection, by their
    /// entity id. Whether one is *using* the connection is asked of the view
    /// itself when the idle check runs: a thread whose tab was closed keeps
    /// its view (and its connection) in the panel's parking slots, so being
    /// alive is not the same as being in use.
    holders: HashMap<EntityId, (ConnectionSlot, WeakEntity<ConversationView>)>,
    /// ADR-F49 (new decision): the hands on a connection that are not views
    /// (see [`Self::lease_connection`]). One is in use while a lease of it is
    /// alive somewhere, which is what `Rc::strong_count` past this one says.
    leases: HashMap<ConnectionSlot, Rc<()>>,
    /// Since when nothing has used each connection, on this session's clock.
    unused_since: HashMap<ConnectionSlot, Instant>,
    /// ADR-F96: connections past a limit that the last view left while they
    /// were still starting, to close once they have started if no view has
    /// come onto them by then.
    left_while_starting: HashSet<ConnectionSlot>,
    /// ADR-F103: see [`Self::read_session_options`].
    session_options_reads: HashMap<Agent, SessionOptionsRead>,
    /// ADR-F101: where each connection's start installed its agent, which an
    /// update agreed to from its thread replaces.
    install_places: HashMap<ConnectionSlot, AgentInstallPlace>,
    idle_watch: Task<()>,
    idle_watch_running: bool,
    _subscriptions: Vec<Subscription>,
}

impl AgentConnectionStore {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let subscriptions = vec![
            cx.subscribe(&agent_server_store, Self::handle_agent_servers_updated),
            // Turning `agent.idle_disconnect_minutes` back on has to start the
            // watch again: it stops itself while the setting says never.
            cx.observe_global::<SettingsStore>(Self::watch_idle_connections),
        ];
        Self {
            project,
            entries: HashMap::default(),
            outside_sandbox: HashMap::default(),
            superseded_outside_sandbox: HashSet::default(),
            next_outside_sandbox_id: 0,
            in_sandbox: HashMap::default(),
            unrestricted_network: HashMap::default(),
            superseded_unrestricted_network: HashSet::default(),
            new_versions: HashMap::default(),
            holders: HashMap::default(),
            leases: HashMap::default(),
            unused_since: HashMap::default(),
            left_while_starting: HashSet::default(),
            session_options_reads: HashMap::default(),
            install_places: HashMap::default(),
            idle_watch: Task::ready(()),
            idle_watch_running: false,
            _subscriptions: subscriptions,
        }
    }

    /// The newer version last reported for `key`, if the running agent told us
    /// about one during this session.
    pub fn new_version_available(&self, key: &Agent) -> Option<&SharedString> {
        self.new_versions.get(key)
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    /// ADR-F101: the user agreed, from a thread's update banner, to update
    /// `agent` where the connection at `place` installed it. The next start
    /// there installs the registry's version, so no start is handed one of
    /// the agent's connections any more: the views that use one keep it until
    /// they start again.
    pub fn begin_agent_update(
        &mut self,
        agent: &Agent,
        place: ConnectionPlace,
        cx: &mut Context<Self>,
    ) {
        if let Agent::Custom { id } = agent {
            let slot = ConnectionSlot::at(agent.clone(), place);
            let install_place = self
                .install_places
                .get(&slot)
                .cloned()
                .unwrap_or_else(|| self.install_place(&slot, cx));
            self.project
                .read(cx)
                .agent_server_store()
                .read(cx)
                .approve_agent_update(id, install_place);
        }
        self.supersede_connections_of(agent, cx);
    }

    /// ADR-F101: where a start for `slot` installs its agent: this host when it
    /// starts outside the sandbox (or on a remote host, whose store keeps its
    /// own agreements), and on Windows the WSL distro of the project's folders
    /// when it starts in the sandbox — as `CustomAgentServer::connect` decides
    /// its launch target.
    fn install_place(&self, slot: &ConnectionSlot, cx: &App) -> AgentInstallPlace {
        let Agent::Custom { id } = slot.agent() else {
            return AgentInstallPlace::Host;
        };
        let project = self.project.read(cx);
        if !project.is_local() {
            return AgentInstallPlace::Host;
        }
        let outside_sandbox = match slot {
            ConnectionSlot::OutsideSandbox { .. } => true,
            ConnectionSlot::InSandbox(_) | ConnectionSlot::UnrestrictedNetwork { .. } => false,
            ConnectionSlot::Shared(_) => {
                agent_servers::runs_outside_sandbox_by_user_choice(id.as_ref(), cx)
            }
        };
        if outside_sandbox {
            return AgentInstallPlace::Host;
        }
        let roots =
            project::agent_sandbox::project_key_roots(project.worktree_store().read(cx), cx);
        match project::agent_sandbox::wsl_distro_for_roots(&roots) {
            Ok(wsl_distro) => {
                AgentInstallPlace::for_launch_target(&AgentLaunchTarget::Sandbox { wsl_distro })
            }
            // That start fails before it installs anything.
            Err(_) => AgentInstallPlace::Host,
        }
    }

    /// ADR-F101: the update check found `version` of `agent` and the user
    /// agreed to it for the places that were behind
    /// ([`crate::agent_updates::approve_agent_updates`]). The threads on its
    /// connections show the update banner, which restarts them on the new
    /// version.
    pub fn announce_agent_update(
        &mut self,
        agent: &Agent,
        version: SharedString,
        cx: &mut Context<Self>,
    ) {
        self.new_versions.insert(agent.clone(), version.clone());
        let entries = self
            .entries
            .get(agent)
            .into_iter()
            .chain(self.in_sandbox.get(agent))
            .chain(
                self.outside_sandbox
                    .values()
                    .chain(self.unrestricted_network.values())
                    .filter(|(key, _)| key == agent)
                    .map(|(_, entry)| entry),
            )
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            let version = version.clone();
            entry.update(cx, |_, cx| {
                cx.emit(AgentConnectionEntryEvent::NewVersionAvailable(version));
            });
        }
        self.supersede_connections_of(agent, cx);
    }

    fn supersede_connections_of(&mut self, agent: &Agent, cx: &mut Context<Self>) {
        // The shared ones go, so that the next request starts anew.
        self.remove_slot_entry(&ConnectionSlot::Shared(agent.clone()));
        self.remove_slot_entry(&ConnectionSlot::InSandbox(agent.clone()));
        // ADR-F49 decision 19 (8c): these views keep using theirs until they
        // start again, and the idle disconnect has to be able to find them
        // until then and after.
        for (id, (key, _)) in &self.outside_sandbox {
            if key == agent {
                self.superseded_outside_sandbox.insert(*id);
            }
        }
        for (id, (key, _)) in &self.unrestricted_network {
            if key == agent {
                self.superseded_unrestricted_network.insert(*id);
            }
        }
        cx.notify();
    }

    pub fn entry(&self, key: &Agent) -> Option<&Entity<AgentConnectionEntry>> {
        self.entries.get(key)
    }

    pub fn connection_status(&self, key: &Agent, cx: &App) -> AgentConnectionStatus {
        self.entries
            .get(key)
            .map(|entry| entry.read(cx).status())
            .unwrap_or(AgentConnectionStatus::Disconnected)
    }

    pub fn agent_version(&self, key: &Agent, cx: &App) -> Option<SharedString> {
        match self.entries.get(key)?.read(cx) {
            AgentConnectionEntry::Connected(state) => state.connection.agent_version(),
            AgentConnectionEntry::Connecting { .. } | AgentConnectionEntry::Error { .. } => None,
        }
    }

    pub fn active_acp_connections(&self, cx: &App) -> Vec<ActiveAcpConnection> {
        self.entries
            .values()
            .chain(self.outside_sandbox.values().map(|(_, entry)| entry))
            .chain(self.in_sandbox.values())
            .chain(self.unrestricted_network.values().map(|(_, entry)| entry))
            .filter_map(|entry| match entry.read(cx) {
                AgentConnectionEntry::Connected(state) => state
                    .connection
                    .clone()
                    .downcast::<AcpConnection>()
                    .map(|connection| ActiveAcpConnection {
                        agent_id: state.connection.agent_id(),
                        connection,
                    }),
                AgentConnectionEntry::Connecting { .. } | AgentConnectionEntry::Error { .. } => {
                    None
                }
            })
            .collect()
    }

    pub fn restart_connection(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        if let Some(entry) = self.entries.get(&key) {
            if matches!(entry.read(cx), AgentConnectionEntry::Connecting { .. }) {
                return entry.clone();
            }
        }

        self.entries.remove(&key);
        self.request_connection(key, server, cx)
    }

    /// [`Self::restart_connection`] for the connection at `place`. ADR-F49
    /// decision 19 (8c): a view outside the sandbox, or in it while the
    /// settings start the agent outside, is not on the shared connection, so
    /// restarting that one would start a process nobody asked for and leave
    /// the view on the connection it wanted to replace. The view's next start
    /// ([`ConversationView`]'s reset) makes the new one, outside the sandbox
    /// only where its thread was confirmed for that.
    pub fn restart_connection_at(
        &mut self,
        key: Agent,
        place: ConnectionPlace,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) {
        let slot = match place {
            ConnectionPlace::AsTheSettingsSay => {
                self.restart_connection(key, server, cx);
                return;
            }
            ConnectionPlace::InSandbox => ConnectionSlot::InSandbox(key),
            ConnectionPlace::OutsideSandbox(id) => {
                ConnectionSlot::OutsideSandbox { id, agent: key }
            }
            ConnectionPlace::UnrestrictedNetwork(id) => {
                ConnectionSlot::UnrestrictedNetwork { id, agent: key }
            }
        };
        if let Some(entry) = self.slot_entry(&slot)
            && matches!(entry.read(cx), AgentConnectionEntry::Connecting { .. })
        {
            return;
        }
        match slot {
            // Kept where the idle disconnect still finds it, as an update does
            // with a connection outside the sandbox.
            ConnectionSlot::OutsideSandbox { id, .. } => {
                self.superseded_outside_sandbox.insert(id);
            }
            ConnectionSlot::UnrestrictedNetwork { id, .. } => {
                self.superseded_unrestricted_network.insert(id);
            }
            slot => self.remove_slot_entry(&slot),
        }
        cx.notify();
    }

    pub fn request_connection(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        self.retire_shared_connection_outside_sandbox(&key, cx);
        if let Some(entry) = self.entries.get(&key) {
            let entry = entry.clone();
            // ADR-F49 (new decision): whoever asked is about to use it, so the
            // grace starts over. Not every caller is a view the idle check can
            // ask later (a thread import listing sessions, an archived thread
            // being deleted on the agent), and this is what keeps the
            // connection under those.
            self.unused_since.remove(&ConnectionSlot::Shared(key));
            return entry;
        }
        self.insert_connection(ConnectionSlot::Shared(key), server, cx)
    }

    /// ADR-F49 decision 19 (8c): a shared connection is outside the sandbox
    /// only because the user's settings said so when it was made. Once they
    /// no longer do, no further start is handed it — a new thread would
    /// otherwise go on starting outside, unasked, after the user turned that
    /// off. The threads already on it keep it (and its banner) and it is set
    /// aside where the idle disconnect still finds it, as an update does with
    /// a connection outside the sandbox.
    fn retire_shared_connection_outside_sandbox(&mut self, key: &Agent, cx: &mut Context<Self>) {
        let Agent::Custom { id: agent_id } = key else {
            return;
        };
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        let AgentConnectionEntry::Connected(state) = entry.read(cx) else {
            return;
        };
        if !state.connection.runs_outside_sandbox()
            || agent_servers::runs_outside_sandbox_by_user_choice(agent_id.0.as_ref(), cx)
        {
            return;
        }
        let Some(entry) = self.entries.remove(key) else {
            return;
        };
        let id = OutsideSandboxConnectionId(self.next_outside_sandbox_id);
        self.next_outside_sandbox_id += 1;
        let shared = ConnectionSlot::Shared(key.clone());
        let retired = ConnectionSlot::OutsideSandbox {
            id,
            agent: key.clone(),
        };
        self.outside_sandbox.insert(id, (key.clone(), entry));
        self.superseded_outside_sandbox.insert(id);
        for (held, _) in self.holders.values_mut() {
            if *held == shared {
                *held = retired.clone();
            }
        }
        if let Some(since) = self.unused_since.remove(&shared) {
            self.unused_since.insert(retired, since);
        }
        log::info!(
            "{} is no longer to start outside its sandbox: the next start goes into it",
            key.id()
        );
    }

    /// ADR-F49 decision 19 (8c): the agent's connection in its sandbox, for a
    /// thread whose session was started there, while the user's settings start
    /// every other start of it outside. Shared by every such thread.
    pub fn request_connection_in_sandbox(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        if let Some(entry) = self.in_sandbox.get(&key) {
            let entry = entry.clone();
            self.unused_since.remove(&ConnectionSlot::InSandbox(key));
            return entry;
        }
        self.insert_connection(ConnectionSlot::InSandbox(key), server, cx)
    }

    /// ADR-F49 decision 19: connects the agent again, started outside its
    /// sandbox this once, after the user confirmed it. The connection is the
    /// confirming view's alone — [`Self::outside_sandbox_connection`] hands
    /// it back to that view; nothing else finds it.
    pub fn connect_outside_sandbox(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> OutsideSandboxConnectionId {
        let id = OutsideSandboxConnectionId(self.next_outside_sandbox_id);
        self.next_outside_sandbox_id += 1;
        self.insert_connection(
            ConnectionSlot::OutsideSandbox { id, agent: key },
            server,
            cx,
        );
        id
    }

    /// The connection `id` names, for a start that wants it: `None` once it
    /// failed to start, the agent went away, the idle disconnect took it or a
    /// newer version of the agent superseded it. The view then starts another
    /// one outside the sandbox where the user confirmed that for its thread,
    /// and goes back into the sandbox otherwise.
    pub fn outside_sandbox_connection(
        &self,
        id: OutsideSandboxConnectionId,
    ) -> Option<Entity<AgentConnectionEntry>> {
        if self.superseded_outside_sandbox.contains(&id) {
            return None;
        }
        self.outside_sandbox
            .get(&id)
            .map(|(_, entry)| entry.clone())
    }

    /// ADR-F96 stage 3: connects the agent again in its sandbox without its
    /// own hosts, so that it reaches any host the editor-wide allowlist
    /// permits. In the sandbox whatever the user's settings say — a start
    /// outside it has no network restriction of its own to lift.
    ///
    /// Only threads that asked for it are handed it, but they share it, as the
    /// threads that keep the agent's own hosts share theirs: every such
    /// connection of an agent in this project starts the same way, so one per
    /// thread would only multiply the agent's processes. One a newer version
    /// superseded, or a reload, is not handed out again.
    pub fn connect_with_unrestricted_network(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> UnrestrictedNetworkConnectionId {
        let running = self
            .unrestricted_network
            .iter()
            .find(|(id, (agent, _))| {
                *agent == key && !self.superseded_unrestricted_network.contains(*id)
            })
            .map(|(id, _)| *id);
        if let Some(id) = running {
            self.unused_since
                .remove(&ConnectionSlot::UnrestrictedNetwork { id, agent: key });
            return id;
        }
        let id = UnrestrictedNetworkConnectionId(self.next_outside_sandbox_id);
        self.next_outside_sandbox_id += 1;
        self.insert_connection(
            ConnectionSlot::UnrestrictedNetwork { id, agent: key },
            server,
            cx,
        );
        id
    }

    /// The connection `id` names, as [`Self::outside_sandbox_connection`]
    /// does for those outside the sandbox.
    pub fn unrestricted_network_connection(
        &self,
        id: UnrestrictedNetworkConnectionId,
    ) -> Option<Entity<AgentConnectionEntry>> {
        if self.superseded_unrestricted_network.contains(&id) {
            return None;
        }
        self.unrestricted_network
            .get(&id)
            .map(|(_, entry)| entry.clone())
    }

    fn slot_entry(&self, slot: &ConnectionSlot) -> Option<&Entity<AgentConnectionEntry>> {
        match slot {
            ConnectionSlot::Shared(key) => self.entries.get(key),
            ConnectionSlot::InSandbox(key) => self.in_sandbox.get(key),
            ConnectionSlot::OutsideSandbox { id, .. } => {
                self.outside_sandbox.get(id).map(|(_, entry)| entry)
            }
            ConnectionSlot::UnrestrictedNetwork { id, .. } => {
                self.unrestricted_network.get(id).map(|(_, entry)| entry)
            }
        }
    }

    fn insert_slot_entry(&mut self, slot: &ConnectionSlot, entry: Entity<AgentConnectionEntry>) {
        match slot {
            ConnectionSlot::Shared(key) => {
                self.entries.insert(key.clone(), entry);
            }
            ConnectionSlot::InSandbox(key) => {
                self.in_sandbox.insert(key.clone(), entry);
            }
            ConnectionSlot::OutsideSandbox { id, agent } => {
                self.outside_sandbox.insert(*id, (agent.clone(), entry));
            }
            ConnectionSlot::UnrestrictedNetwork { id, agent } => {
                self.unrestricted_network
                    .insert(*id, (agent.clone(), entry));
            }
        }
    }

    fn remove_slot_entry(&mut self, slot: &ConnectionSlot) {
        self.left_while_starting.remove(slot);
        match slot {
            ConnectionSlot::Shared(key) => {
                self.entries.remove(key);
            }
            ConnectionSlot::InSandbox(key) => {
                self.in_sandbox.remove(key);
            }
            ConnectionSlot::OutsideSandbox { id, .. } => {
                self.outside_sandbox.remove(id);
                self.superseded_outside_sandbox.remove(id);
            }
            ConnectionSlot::UnrestrictedNetwork { id, .. } => {
                self.unrestricted_network.remove(id);
                self.superseded_unrestricted_network.remove(id);
            }
        }
    }

    fn insert_connection(
        &mut self,
        slot: ConnectionSlot,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        // ADR-F101: a start that still launches an older version than the
        // registry's says so again itself.
        self.new_versions.remove(slot.agent());
        let install_place = self.install_place(&slot, cx);
        self.install_places.insert(slot.clone(), install_place);
        let (mut new_version_rx, mut loading_status_rx, connect_task) =
            self.start_connection(server, &slot, cx);
        let connect_task = connect_task.shared();

        let entry = cx.new(|_cx| AgentConnectionEntry::Connecting {
            connect_task: connect_task.clone(),
        });

        self.insert_slot_entry(&slot, entry.clone());
        self.unused_since.remove(&slot);
        self.watch_idle_connections(cx);
        cx.notify();

        cx.spawn({
            let slot = slot.clone();
            let entry = entry.downgrade();
            async move |this, cx| match connect_task.await {
                Ok(connected_state) => {
                    this.update(cx, move |this, cx| {
                        if this.slot_entry(&slot) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |entry, cx| {
                                if let AgentConnectionEntry::Connecting { .. } = entry {
                                    *entry = AgentConnectionEntry::Connected(connected_state);
                                    cx.notify();
                                }
                            })
                            .ok();
                        if this.left_while_starting.remove(&slot) {
                            this.close_connection_of_one_thread_nobody_holds(&slot, cx);
                        }
                        cx.notify();
                    })
                    .ok();
                }
                Err(error) => {
                    this.update(cx, move |this, cx| {
                        if this.slot_entry(&slot) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |entry, cx| {
                                if let AgentConnectionEntry::Connecting { .. } = entry {
                                    *entry = AgentConnectionEntry::Error { error };
                                    cx.notify();
                                }
                            })
                            .ok();
                        this.remove_slot_entry(&slot);
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        cx.spawn({
            let slot = slot.clone();
            let entry = entry.downgrade();
            async move |this, cx| {
                while let Ok(version) = new_version_rx.recv().await {
                    let Some(version) = version else {
                        continue;
                    };

                    this.update(cx, move |this, cx| {
                        if this.slot_entry(&slot) != entry.upgrade().as_ref() {
                            return;
                        }

                        // ADR-F101: the connection stays. A start without the
                        // user's agreement launches the version installed
                        // and reports the newer one again, so dropping it
                        // here would only start that same version once more
                        // for every thread; the update replaces it
                        // ([`Self::begin_agent_update`]).
                        let version = SharedString::from(version);
                        this.new_versions
                            .insert(slot.agent().clone(), version.clone());
                        entry
                            .update(cx, move |_entry, cx| {
                                cx.emit(AgentConnectionEntryEvent::NewVersionAvailable(version));
                            })
                            .ok();
                        cx.notify();
                    })
                    .ok();
                    break;
                }
            }
        })
        .detach();

        cx.spawn({
            let entry = entry.downgrade();
            async move |this, cx| {
                while let Ok(status) = loading_status_rx.recv().await {
                    let status = status.map(SharedString::from);
                    let slot = slot.clone();
                    let entry = entry.clone();
                    this.update(cx, move |this, cx| {
                        if this.slot_entry(&slot) != entry.upgrade().as_ref() {
                            return;
                        }

                        entry
                            .update(cx, move |_entry, cx| {
                                cx.emit(AgentConnectionEntryEvent::LoadingStatusChanged(status));
                            })
                            .ok();
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();

        entry
    }

    /// ADR-F49 (new decision): `view` needs this agent's connection, and the
    /// connection is kept for as long as the view uses it. Called again every
    /// time the view changes which connection it is on (a start outside the
    /// sandbox moves it to one of its own), which replaces the earlier claim.
    pub fn hold_connection(
        &mut self,
        view: WeakEntity<ConversationView>,
        key: &Agent,
        outside_sandbox: Option<OutsideSandboxConnectionId>,
        cx: &mut Context<Self>,
    ) {
        let place = match outside_sandbox {
            Some(id) => ConnectionPlace::OutsideSandbox(id),
            None => ConnectionPlace::AsTheSettingsSay,
        };
        self.hold_connection_at(view, key, place, cx);
    }

    /// [`Self::hold_connection`] for a connection at `place`.
    pub fn hold_connection_at(
        &mut self,
        view: WeakEntity<ConversationView>,
        key: &Agent,
        place: ConnectionPlace,
        cx: &mut Context<Self>,
    ) {
        let slot = match place {
            ConnectionPlace::OutsideSandbox(id) => ConnectionSlot::OutsideSandbox {
                id,
                agent: key.clone(),
            },
            ConnectionPlace::AsTheSettingsSay => ConnectionSlot::Shared(key.clone()),
            ConnectionPlace::InSandbox => ConnectionSlot::InSandbox(key.clone()),
            ConnectionPlace::UnrestrictedNetwork(id) => ConnectionSlot::UnrestrictedNetwork {
                id,
                agent: key.clone(),
            },
        };
        self.unused_since.remove(&slot);
        self.holders.retain(|_, (_, view)| view.upgrade().is_some());
        let left = self
            .holders
            .insert(view.entity_id(), (slot.clone(), view))
            .map(|(left, _)| left)
            .filter(|left| *left != slot);
        if let Some(left) = left {
            self.close_connection_of_one_thread_nobody_holds(&left, cx);
        }
        self.watch_idle_connections(cx);
    }

    /// ADR-F96: a connection without the agent's own hosts (stage 3), or one
    /// outside the sandbox, is a way past a limit the user asked to lift for
    /// particular threads, so once the last view on it has moved off — its
    /// thread was restricted again or went back into the sandbox, or the
    /// setting that allows it was turned off — it is closed then rather than
    /// at the idle disconnect. One still starting is looked at again once it
    /// has started; one with processes of an answer still running in its
    /// sandbox is left to the idle disconnect, which waits for those.
    fn close_connection_of_one_thread_nobody_holds(
        &mut self,
        slot: &ConnectionSlot,
        cx: &mut Context<Self>,
    ) {
        if !matches!(
            slot,
            ConnectionSlot::UnrestrictedNetwork { .. } | ConnectionSlot::OutsideSandbox { .. }
        ) || self
            .holders
            .values()
            .any(|(held, view)| held == slot && view.upgrade().is_some())
        {
            return;
        }
        let Some(entry) = self.slot_entry(slot).cloned() else {
            return;
        };
        let AgentConnectionEntry::Connected(state) = entry.read(cx) else {
            self.left_while_starting.insert(slot.clone());
            return;
        };
        if let Some(monitor) = state.connection.sandbox_process_monitor()
            && let Some(reason) = monitor.read(cx).reason_to_hold_an_unused_connection()
        {
            log::info!(
                "keeping the connection to {} no thread is on until the idle disconnect: {reason}",
                slot.agent().id()
            );
            return;
        }
        log::info!(
            "closing the connection to {} that let threads past a limit: no thread is on it",
            slot.agent().id()
        );
        self.remove_slot_entry(slot);
        self.unused_since.remove(slot);
        cx.notify();
    }

    /// ADR-F96: the view `view_id` is gone, so the connection it was on loses
    /// its hand, and one past a limit closes if no other view is on it
    /// ([`Self::close_connection_of_one_thread_nobody_holds`]).
    pub fn release_connection_holder(&mut self, view_id: EntityId, cx: &mut Context<Self>) {
        if let Some((left, _)) = self.holders.remove(&view_id) {
            self.close_connection_of_one_thread_nobody_holds(&left, cx);
        }
    }

    /// ADR-F49 (new decision): a hand on this agent's connection that is not a
    /// [`ConversationView`] — a thread import listing the agent's sessions, an
    /// archived thread being deleted on the agent. The idle check cannot ask
    /// such a caller whether it is done, and dropping the connection under one
    /// would leave the agent's process running with nothing in the store
    /// pointing at it, so the next request would start a second one. Holding
    /// the lease keeps the connection; dropping it gives it back.
    #[must_use = "the connection is only kept while the lease is alive"]
    pub fn lease_connection(&mut self, key: &Agent) -> ConnectionLease {
        self.lease_slot(ConnectionSlot::Shared(key.clone()))
    }

    fn lease_slot(&mut self, slot: ConnectionSlot) -> ConnectionLease {
        self.unused_since.remove(&slot);
        ConnectionLease(self.leases.entry(slot).or_default().clone())
    }

    /// ADR-F103: reads what a session of `server`'s agent offers — its model
    /// and other options, its modes and its slash commands — in a session of
    /// its own, and writes it down as ADR-F100's snapshot, so that a new
    /// thread can offer them before the first one of the agent has started.
    ///
    /// The agent is started in the safest place whatever the thread's
    /// switches say: in its sandbox, keeping to its own hosts. That is the
    /// shared connection, unless the user's settings start the agent outside
    /// it, which a start in the sandbox then overrides. The session is given
    /// no MCP servers, nothing is sent in it, and it is closed once read; the
    /// connection goes with it unless a thread has come onto it meanwhile.
    pub fn read_session_options(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> SessionOptionsRead {
        if let Some(read) = self.session_options_reads.get(&key) {
            return read.clone();
        }
        let agent_id = server.agent_id();
        let (slot, entry) =
            if agent_servers::runs_outside_sandbox_by_user_choice(agent_id.0.as_ref(), cx) {
                (
                    ConnectionSlot::InSandbox(key.clone()),
                    self.request_connection_in_sandbox(key.clone(), server, cx),
                )
            } else {
                (
                    ConnectionSlot::Shared(key.clone()),
                    self.request_connection(key.clone(), server, cx),
                )
            };
        let lease = self.lease_slot(slot.clone());
        let connected = entry.read(cx).wait_for_connection();
        let entry = entry.downgrade();
        let project = self.project.clone();
        let task = cx.spawn({
            let key = key.clone();
            async move |this, cx| {
                let result = read_session_options_in_a_session_of_their_own(
                    connected, agent_id, project, cx,
                )
                .await;
                drop(lease);
                this.update(cx, |this, cx| {
                    match &result {
                        Ok(()) => {
                            this.session_options_reads.remove(&key);
                        }
                        Err(error) => {
                            log::error!(
                                "could not read the options of {} before its first thread: {error}",
                                key.id()
                            );
                            this.session_options_reads
                                .insert(key.clone(), SessionOptionsRead::Failed);
                        }
                    }
                    this.close_connection_that_only_read_options(&slot, &entry, cx);
                    cx.notify();
                })
                .log_err();
                result
            }
        });
        let read = SessionOptionsRead::Reading(task.shared());
        self.session_options_reads.insert(key, read.clone());
        read
    }

    /// ADR-F103: the connection [`Self::read_session_options`] started goes
    /// once the read is over, unless a thread came onto it in the meantime
    /// (the user sent the first message while the options were read) or
    /// something else still holds it.
    fn close_connection_that_only_read_options(
        &mut self,
        slot: &ConnectionSlot,
        entry: &WeakEntity<AgentConnectionEntry>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = entry.upgrade() else {
            return;
        };
        if self.slot_entry(slot) != Some(&entry) {
            return;
        }
        self.holders.retain(|_, (_, view)| view.upgrade().is_some());
        self.leases.retain(|_, lease| Rc::strong_count(lease) > 1);
        if self.holders.values().any(|(held, _)| held == slot) || self.leases.contains_key(slot) {
            return;
        }
        if let AgentConnectionEntry::Connected(state) = entry.read(cx)
            && let Some(monitor) = state.connection.sandbox_process_monitor()
            && let Some(reason) = monitor.read(cx).reason_to_hold_an_unused_connection()
        {
            log::info!(
                "keeping the connection to {} that read its options until the idle disconnect: {reason}",
                slot.agent().id()
            );
            return;
        }
        log::info!(
            "closing the connection to {} that only read its options: no thread is on it",
            slot.agent().id()
        );
        self.remove_slot_entry(slot);
        self.unused_since.remove(slot);
    }

    /// Keeps the idle check running while there is a connection it could drop.
    fn watch_idle_connections(&mut self, cx: &mut Context<Self>) {
        if self.idle_watch_running
            || AgentSettings::get_global(cx).idle_disconnect().is_none()
            || self.idle_candidates().is_empty()
        {
            return;
        }
        self.idle_watch_running = true;
        self.idle_watch = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(IDLE_CHECK_INTERVAL).await;
                let Ok(keep_watching) = this.update(cx, |this, cx| this.sweep_idle_connections(cx))
                else {
                    return;
                };
                if !keep_watching {
                    return;
                }
            }
        });
    }

    /// Drops every connection whose grace has run out, and says whether the
    /// watch has anything left to look at.
    fn sweep_idle_connections(&mut self, cx: &mut Context<Self>) -> bool {
        self.holders.retain(|_, (_, view)| view.upgrade().is_some());

        let Some(grace) = AgentSettings::get_global(cx).idle_disconnect() else {
            self.unused_since.clear();
            self.idle_watch_running = false;
            return false;
        };

        self.leases.retain(|_, lease| Rc::strong_count(lease) > 1);

        let now = cx.background_executor().now();
        let candidates = self.idle_candidates();
        for slot in candidates {
            if self.slot_is_in_use(&slot, cx) {
                self.unused_since.remove(&slot);
                continue;
            }
            let unused_since = *self.unused_since.entry(slot.clone()).or_insert(now);
            if now.saturating_duration_since(unused_since) < grace {
                continue;
            }
            self.drop_idle_connection(&slot, grace, cx);
        }

        let left = self
            .idle_candidates()
            .into_iter()
            .collect::<collections::HashSet<_>>();
        self.unused_since.retain(|slot, _| left.contains(slot));
        self.idle_watch_running = !left.is_empty();
        self.idle_watch_running
    }

    /// The connections the idle disconnect applies to: every external agent
    /// this store has a connection for.
    fn idle_candidates(&self) -> Vec<ConnectionSlot> {
        self.entries
            .keys()
            .map(|key| ConnectionSlot::Shared(key.clone()))
            .chain(
                self.in_sandbox
                    .keys()
                    .map(|key| ConnectionSlot::InSandbox(key.clone())),
            )
            .chain(self.outside_sandbox.iter().map(|(id, (agent, _))| {
                ConnectionSlot::OutsideSandbox {
                    id: *id,
                    agent: agent.clone(),
                }
            }))
            .chain(self.unrestricted_network.iter().map(|(id, (agent, _))| {
                ConnectionSlot::UnrestrictedNetwork {
                    id: *id,
                    agent: agent.clone(),
                }
            }))
            .filter(ConnectionSlot::can_be_dropped_when_idle)
            .collect()
    }

    fn slot_is_in_use(&self, slot: &ConnectionSlot, cx: &App) -> bool {
        // A connection that has not finished starting is one somebody asked
        // for and is waiting on, whether or not that somebody is a view this
        // could ask: dropping it here would leave the agent coming up with
        // nothing in the store pointing at it.
        if self
            .slot_entry(slot)
            .is_none_or(|entry| !matches!(entry.read(cx), AgentConnectionEntry::Connected(_)))
        {
            return true;
        }
        if self
            .leases
            .get(slot)
            .is_some_and(|lease| Rc::strong_count(lease) > 1)
        {
            return true;
        }
        self.holders.values().any(|(held, view)| {
            held == slot
                && view
                    .upgrade()
                    .is_some_and(|view| view.read(cx).is_using_connection(cx))
        })
    }

    /// Ends a connection nothing has used for `grace`, unless something still
    /// speaks for it.
    fn drop_idle_connection(
        &mut self,
        slot: &ConnectionSlot,
        grace: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.slot_entry(slot) else {
            return;
        };

        // ADR-F49 decision 18 (C): what an answer left running in the sandbox
        // keeps the agent a writer until it ends, and this connection is what
        // follows it and what the dialog's stop button reaches. Dropping it
        // would take the pause and the list away while the processes run on,
        // so an agent with something behind it is left alone and looked at
        // again at the next check, going as soon as the last of them ends.
        // Which sandbox is which is the monitor's to answer.
        if let AgentConnectionEntry::Connected(state) = entry.read(cx)
            && let Some(monitor) = state.connection.sandbox_process_monitor()
            && let Some(reason) = monitor.read(cx).reason_to_hold_an_unused_connection()
        {
            log::info!(
                "keeping the unused connection to {}: {reason}",
                slot.agent().id()
            );
            return;
        }

        // A view that is not using the connection is still holding it, and the
        // agent only stops once every hand has let go. Every hand is asked
        // before any of them lets go: one that cannot means the connection
        // stays, and the others would then have been parked out of the panel
        // for nothing.
        let holders = self
            .holders
            .values()
            .filter(|(held, _)| held == slot)
            .filter_map(|(_, view)| view.upgrade())
            .collect::<Vec<_>>();
        if !holders.iter().all(|view| {
            view.read(cx)
                .can_release_idle_connection(view.entity_id(), cx)
        }) {
            return;
        }
        for view in holders {
            if !ConversationView::release_idle_connection(&view, cx) {
                return;
            }
        }

        log::info!(
            "dropping the connection to {}: no thread has used it for {grace:?}",
            slot.agent().id()
        );
        self.remove_slot_entry(slot);
        self.unused_since.remove(slot);
        cx.notify();
    }

    fn handle_agent_servers_updated(
        &mut self,
        store: Entity<AgentServerStore>,
        _: &AgentServersUpdated,
        cx: &mut Context<Self>,
    ) {
        let store = store.read(cx);
        let still_there = |key: &Agent| match key {
            Agent::NativeAgent => true,
            Agent::Custom { id } => store.external_agents.contains_key(id),
            #[cfg(any(test, feature = "test-support"))]
            Agent::Stub => true,
        };
        self.entries.retain(|key, _| still_there(key));
        self.in_sandbox.retain(|key, _| still_there(key));
        self.outside_sandbox.retain(|_, (key, _)| still_there(key));
        let outside_sandbox = &self.outside_sandbox;
        self.superseded_outside_sandbox
            .retain(|id| outside_sandbox.contains_key(id));
        self.unrestricted_network
            .retain(|_, (key, _)| still_there(key));
        let unrestricted_network = &self.unrestricted_network;
        self.superseded_unrestricted_network
            .retain(|id| unrestricted_network.contains_key(id));
        self.unused_since
            .retain(|slot, _| still_there(slot.agent()));
        self.left_while_starting
            .retain(|slot| still_there(slot.agent()));
        cx.notify();
    }

    fn start_connection(
        &self,
        server: Rc<dyn AgentServer>,
        slot: &ConnectionSlot,
        cx: &mut Context<Self>,
    ) -> (
        Receiver<Option<String>>,
        Receiver<Option<String>>,
        Task<Result<AgentConnectedState, LoadError>>,
    ) {
        let (new_version_tx, new_version_rx) = watch::channel::<Option<String>>(None);
        let (loading_status_tx, loading_status_rx) = watch::channel::<Option<String>>(None);

        let agent_server_store = self.project.read(cx).agent_server_store().clone();
        let mut delegate = AgentServerDelegate::new(
            agent_server_store,
            Some(new_version_tx),
            Some(loading_status_tx),
        );
        match slot {
            ConnectionSlot::Shared(_) => {}
            ConnectionSlot::OutsideSandbox { .. } => {
                delegate = delegate.outside_sandbox_once();
            }
            ConnectionSlot::InSandbox(_) => {
                delegate = delegate.in_sandbox_whatever_the_settings();
            }
            ConnectionSlot::UnrestrictedNetwork { .. } => {
                delegate = delegate
                    .in_sandbox_whatever_the_settings()
                    .lift_agent_hosts();
            }
        }

        let connect_task = server.connect(delegate, self.project.clone(), cx);
        let connect_task = cx.spawn(async move |_this, _cx| match connect_task.await {
            Ok(connection) => Ok(AgentConnectedState { connection }),
            Err(err) => match err.downcast::<LoadError>() {
                Ok(load_error) => Err(load_error),
                Err(err) => Err(LoadError::Other(SharedString::from(err.to_string()))),
            },
        });
        (new_version_rx, loading_status_rx, connect_task)
    }
}

/// ADR-F103: see [`AgentConnectionStore::read_session_options`].
async fn read_session_options_in_a_session_of_their_own(
    connected: Shared<Task<Result<AgentConnectedState, LoadError>>>,
    agent_id: project::AgentId,
    project: Entity<Project>,
    cx: &mut AsyncApp,
) -> Result<(), SharedString> {
    let connection = connected
        .await
        .map_err(|error| SharedString::from(error.to_string()))?
        .connection;
    let thread = cx
        .update(|cx| {
            let work_dirs = project.read(cx).default_path_list(cx);
            connection
                .clone()
                .new_session_to_read_options(project.clone(), work_dirs, cx)
        })
        .await
        .map_err(|error| SharedString::from(format!("{error:#}")))?;
    let (commands_announced, mut commands_announced_rx) = mpsc::unbounded();
    let _commands_subscription = cx.update(|cx| {
        cx.subscribe(&thread, move |_, event: &AcpThreadEvent, _| {
            if let AcpThreadEvent::AvailableCommandsUpdated(commands) = event
                && !commands.is_empty()
            {
                commands_announced.unbounded_send(()).ok();
            }
        })
    });
    if thread.read_with(cx, |thread, _| thread.available_commands().is_empty()) {
        let timeout = cx
            .background_executor()
            .timer(SESSION_OPTIONS_COMMANDS_WAIT);
        futures::future::select(commands_announced_rx.next(), timeout).await;
    }
    cx.update(|cx| {
        // A thread that started while the options were read wrote what its
        // own session offers, which is what the next thread is to offer.
        if crate::thread_metadata_store::last_session_options(&agent_id, cx)
            .is_some_and(|snapshot| !snapshot.read_before_start)
        {
            return;
        }
        let mut snapshot = SessionOptionsSnapshot::of_session(&thread, cx);
        snapshot.read_before_start = true;
        crate::thread_metadata_store::record_last_session_options(agent_id, snapshot, cx);
    });
    // The session is closed with its thread, which only this holds.
    drop(thread);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::init_test;
    use acp_thread::{
        AcpThread, SandboxProcessMonitor, SandboxProcessSource, SandboxProcessTracking,
    };
    use agent_client_protocol::schema::v1 as acp;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::AgentId;
    use std::any::Any;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::Arc;
    use util::path;
    use workspace::PathList;

    /// ADR-F49 decision 18 (C): a sandbox whose processes a test writes down.
    struct TestSandbox {
        running: RefCell<Vec<sandbox::SandboxProcess>>,
        tracking: SandboxProcessTracking,
        /// The trusted side cannot read this one, which is what puts a sandbox
        /// that has a route at all into case B.
        listing_fails: bool,
    }

    impl TestSandbox {
        fn new(pids: &[u32]) -> Rc<Self> {
            let sandbox = Rc::new(Self {
                running: RefCell::default(),
                tracking: SandboxProcessTracking::Trackable,
                listing_fails: false,
            });
            for pid in pids {
                sandbox.start(*pid);
            }
            sandbox
        }

        /// A WSL distro or a remote host that did not answer what runs in the
        /// sandbox it built: a PID namespace all the same, and one that ends
        /// with the connection.
        fn that_cannot_be_listed() -> Rc<Self> {
            Rc::new(Self {
                running: RefCell::default(),
                tracking: SandboxProcessTracking::Trackable,
                listing_fails: true,
            })
        }

        /// macOS's Seatbelt: no boundary to read and none to end, so what it
        /// holds outlives the agent.
        fn that_cannot_be_looked_into() -> Rc<Self> {
            Rc::new(Self {
                running: RefCell::default(),
                tracking: SandboxProcessTracking::Untrackable,
                listing_fails: true,
            })
        }

        fn start(&self, pid: u32) {
            self.running.borrow_mut().push(sandbox::SandboxProcess {
                pid,
                start_time: u64::from(pid) * 10,
                running_for: Some(Duration::from_secs(3)),
                command: format!("process-{pid}"),
            });
        }

        fn end(&self, pid: u32) {
            self.running
                .borrow_mut()
                .retain(|process| process.pid != pid);
        }
    }

    impl SandboxProcessSource for TestSandbox {
        fn tracking(&self) -> SandboxProcessTracking {
            self.tracking
        }

        fn list(&self, _cx: &App) -> Task<Result<Vec<sandbox::SandboxProcess>>> {
            if self.listing_fails {
                return Task::ready(Err(anyhow::anyhow!(
                    "the host did not say what runs in the agent's sandbox"
                )));
            }
            Task::ready(Ok(self.running.borrow().clone()))
        }

        fn stop(
            &self,
            _processes: Vec<sandbox::SandboxProcessKey>,
            _cx: &App,
        ) -> Task<Result<usize>> {
            Task::ready(Ok(0))
        }

        fn write_scope(&self) -> Option<Vec<PathBuf>> {
            Some(vec![PathBuf::from(path!("/test"))])
        }

        fn agent(&self) -> SharedString {
            "sandboxed-stub".into()
        }
    }

    /// An agent whose sandbox is followed, so that the idle disconnect has to
    /// reckon with what its answers left running.
    struct SandboxedStub {
        sandbox_processes: Option<Entity<SandboxProcessMonitor>>,
    }

    impl AgentConnection for SandboxedStub {
        fn agent_id(&self) -> AgentId {
            AgentId::new("sandboxed-stub")
        }

        fn telemetry_id(&self) -> SharedString {
            "sandboxed-stub".into()
        }

        fn sandbox_process_monitor(&self) -> Option<Entity<SandboxProcessMonitor>> {
            self.sandbox_processes.clone()
        }

        fn new_session(
            self: Rc<Self>,
            _project: Entity<Project>,
            _work_dirs: PathList,
            _cx: &mut App,
        ) -> Task<Result<Entity<AcpThread>>> {
            Task::ready(Err(anyhow::anyhow!("this test needs no thread")))
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn authenticate(&self, _method: acp::AuthMethodId, _cx: &mut App) -> Task<Result<()>> {
            Task::ready(Ok(()))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<Result<acp::PromptResponse>> {
            Task::ready(Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) {}

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    thread_local! {
        /// The sandbox the next connection is given. `AgentServer` has to be
        /// `Send`, and a sandbox a test writes down is not, so it is left here
        /// rather than in the server.
        static TEST_SANDBOX: RefCell<Option<Rc<TestSandbox>>> = const { RefCell::new(None) };
    }

    struct SandboxedStubServer;

    impl AgentServer for SandboxedStubServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("sandboxed-stub")
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            project: Entity<Project>,
            cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            let sandbox = TEST_SANDBOX
                .with(|sandbox| sandbox.borrow().clone())
                .expect("the test says what runs in the sandbox");
            let sandbox_processes = SandboxProcessMonitor::new(sandbox, &project, cx);
            Task::ready(Ok(Rc::new(SandboxedStub { sandbox_processes })))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// An agent whose start never finishes, for the wait the idle check has to
    /// leave alone.
    struct NeverConnectsServer;

    impl AgentServer for NeverConnectsServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("sandboxed-stub")
        }

        fn connect(
            &self,
            _delegate: AgentServerDelegate,
            _project: Entity<Project>,
            cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            cx.spawn(async move |_cx| futures::future::pending().await)
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    fn connect_to_a_sandboxed_stub(
        store: &Entity<AgentConnectionStore>,
        sandbox: Rc<TestSandbox>,
        cx: &mut TestAppContext,
    ) {
        TEST_SANDBOX.with(|slot| *slot.borrow_mut() = Some(sandbox));
        store.update(cx, |store, cx| {
            store.request_connection(sandboxed_stub(), Rc::new(SandboxedStubServer), cx);
        });
        cx.run_until_parked();
    }

    async fn setup(cx: &mut TestAppContext) -> (Entity<AgentConnectionStore>, Entity<Project>) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/test"), serde_json::json!({ "file.txt": "" }))
            .await;
        let project = Project::test(fs, [path!("/test").as_ref()], cx).await;
        let store = cx.update(|cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
        (store, project)
    }

    fn sandboxed_stub() -> Agent {
        Agent::Custom {
            id: AgentId::new("sandboxed-stub"),
        }
    }

    fn is_connected(store: &Entity<AgentConnectionStore>, cx: &mut TestAppContext) -> bool {
        store.read_with(cx, |store, _cx| store.entry(&sandboxed_stub()).is_some())
    }

    /// ADR-F49 decision 18 (C): a connection nothing uses any more, but whose
    /// sandbox still holds something an answer left running, is what the pause
    /// and the dialog's stop button hang from — so the idle disconnect waits
    /// for that to end rather than taking them away with it.
    #[gpui::test]
    async fn test_leftover_sandbox_processes_hold_an_unused_connection(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        // Process 1 is the agent's own, which the sandbox's baseline is taken
        // from; nothing about it is a leftover.
        let sandbox = TestSandbox::new(&[1]);
        connect_to_a_sandboxed_stub(&store, sandbox.clone(), cx);
        assert!(is_connected(&store, cx));

        // An answer leaves a development server of its own behind.
        sandbox.start(2);
        cx.executor().advance_clock(Duration::from_secs(30));
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            is_connected(&store, cx),
            "a connection whose sandbox still has something running is kept, \
             unused or not"
        );

        // Once that ends, the next look lets the connection go.
        sandbox.end(2);
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            !is_connected(&store, cx),
            "with nothing left running, the unused connection goes"
        );
    }

    /// ADR-F49 (new decision): a sandbox whose listing the host never answered
    /// is in case B by accident rather than by nature. WSL and a remote host
    /// both build a PID namespace that this connection holds open, so ending
    /// the connection ends what is inside it — there is nothing left for the
    /// pause to protect, and keeping the connection would only keep the agent.
    #[gpui::test]
    async fn test_an_unlistable_sandbox_that_ends_with_its_connection_is_dropped(
        cx: &mut TestAppContext,
    ) {
        let (store, _project) = setup(cx).await;
        connect_to_a_sandboxed_stub(&store, TestSandbox::that_cannot_be_listed(), cx);
        assert!(is_connected(&store, cx));

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            !is_connected(&store, cx),
            "a sandbox nobody could list still ends with its connection, so the              unused connection goes and takes the agent with it"
        );
    }

    /// macOS's Seatbelt (case B by nature) has no such boundary: what the
    /// agent left behind outlives the connection, so dropping it would take
    /// the pause away from processes that are still there.
    #[gpui::test]
    async fn test_a_sandbox_that_outlives_its_connection_is_kept(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        connect_to_a_sandboxed_stub(&store, TestSandbox::that_cannot_be_looked_into(), cx);

        cx.executor().advance_clock(Duration::from_secs(60 * 60));
        cx.run_until_parked();
        assert!(
            is_connected(&store, cx),
            "a sandbox this side can neither read nor end keeps its connection"
        );
    }

    /// A connection nothing has a view for — a thread import reading an
    /// agent's sessions, an archived thread being deleted on it — is held by
    /// its lease. Dropping it under one would leave the agent's process
    /// running with nothing in the store pointing at it, so the next request
    /// would start a second one.
    #[gpui::test]
    async fn test_a_lease_holds_a_connection_no_view_speaks_for(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        connect_to_a_sandboxed_stub(&store, TestSandbox::new(&[1]), cx);
        let lease = store.update(cx, |store, _cx| store.lease_connection(&sandboxed_stub()));

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            is_connected(&store, cx),
            "a leased connection is in use, whether or not a view says so"
        );

        drop(lease);
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            !is_connected(&store, cx),
            "with the lease given back, the connection goes like any other"
        );
    }

    /// An agent that is still starting — a first run that is installing itself
    /// can take minutes — is one somebody asked for and is waiting on.
    #[gpui::test]
    async fn test_a_connection_that_is_still_starting_is_not_dropped(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        store.update(cx, |store, cx| {
            store.request_connection(sandboxed_stub(), Rc::new(NeverConnectsServer), cx);
        });
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            is_connected(&store, cx),
            "an agent that has not finished starting is not an idle one"
        );
    }

    /// An agent started outside its sandbox whose newer version a test can
    /// report through the sender its start was handed.
    struct ReportsNewVersionServer {
        new_version: Arc<parking_lot::Mutex<Option<watch::Sender<Option<String>>>>>,
    }

    impl AgentServer for ReportsNewVersionServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("sandboxed-stub")
        }

        fn connect(
            &self,
            mut delegate: AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            *self.new_version.lock() = delegate.take_new_version_sender();
            Task::ready(Ok(Rc::new(
                acp_thread::StubAgentConnection::new().with_runs_outside_sandbox(true),
            )))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    /// ADR-F49 decision 19 (8c): a newer version of an agent started outside
    /// its sandbox is not handed to another start, but the connection stays
    /// where the idle disconnect looks, so that the process goes once nothing
    /// uses it. Taking it out at once left it to run for as long as its view
    /// held it, with the sweep blind to it.
    #[gpui::test]
    async fn test_a_superseded_connection_outside_the_sandbox_still_goes_when_unused(
        cx: &mut TestAppContext,
    ) {
        let (store, _project) = setup(cx).await;
        let new_version = Arc::new(parking_lot::Mutex::new(None));
        let id = store.update(cx, |store, cx| {
            store.connect_outside_sandbox(
                sandboxed_stub(),
                Rc::new(ReportsNewVersionServer {
                    new_version: new_version.clone(),
                }),
                cx,
            )
        });
        cx.run_until_parked();
        let connection = store.read_with(cx, |store, cx| {
            match store
                .outside_sandbox_connection(id)
                .expect("connected")
                .read(cx)
            {
                AgentConnectionEntry::Connected(state) => Rc::downgrade(&state.connection),
                _ => panic!("the stub connects at once"),
            }
        });

        let mut sender = new_version.lock().take().expect("the start was handed one");
        sender
            .send(Some("2.0.0".to_string()))
            .expect("the store listens");
        cx.run_until_parked();
        assert!(
            store.read_with(cx, |store, _cx| store
                .outside_sandbox_connection(id)
                .is_some()),
            "ADR-F101: a report alone replaces nothing; the next start launches the \
             installed version anyway"
        );
        // What agreeing to the update does.
        store.update(cx, |store, cx| {
            store.begin_agent_update(&sandboxed_stub(), ConnectionPlace::OutsideSandbox(id), cx)
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _cx| {
            assert!(
                store.outside_sandbox_connection(id).is_none(),
                "no start is handed the old version"
            );
            assert!(
                store.outside_sandbox.contains_key(&id),
                "the idle disconnect can still find it"
            );
            assert_eq!(
                store
                    .new_version_available(&sandboxed_stub())
                    .map(|version| version.as_ref()),
                Some("2.0.0")
            );
        });
        assert!(connection.upgrade().is_some(), "the agent still runs");

        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        assert!(
            store.read_with(cx, |store, _cx| !store.outside_sandbox.contains_key(&id)),
            "nothing used it, so it went"
        );
        assert!(
            connection.upgrade().is_none(),
            "and the agent's process with it"
        );
        drop(sender);
    }

    /// ADR-F101: agreeing to an update from a thread is told to the store that
    /// starts the agent, for the place that thread's connection installed it
    /// in: this host for a start outside the sandbox, and on Windows the WSL
    /// distro for one in it. The other place's install is left alone.
    #[gpui::test]
    async fn test_agreeing_to_an_update_marks_it_for_the_next_start_there(cx: &mut TestAppContext) {
        let (store, project) = setup(cx).await;
        let agent_id = AgentId::new("claude-acp");
        let agent = Agent::Custom {
            id: agent_id.clone(),
        };
        let host = AgentInstallPlace::Host;
        let sandbox =
            AgentInstallPlace::for_launch_target(&AgentLaunchTarget::Sandbox { wsl_distro: None });
        let approved = |place: &AgentInstallPlace, cx: &mut TestAppContext| {
            project.read_with(cx, |project, cx| {
                project
                    .agent_server_store()
                    .read(cx)
                    .agent_update_approved(&agent_id, place)
            })
        };
        assert!(!approved(&host, cx));
        assert!(!approved(&sandbox, cx));

        store.update(cx, |store, cx| {
            store.begin_agent_update(
                &agent,
                ConnectionPlace::OutsideSandbox(OutsideSandboxConnectionId(0)),
                cx,
            )
        });
        assert!(approved(&host, cx));
        if cfg!(target_os = "windows") {
            assert!(
                !approved(&sandbox, cx),
                "the distro's install is not the one outside"
            );
        }

        store.update(cx, |store, cx| {
            store.begin_agent_update(&agent, ConnectionPlace::InSandbox, cx)
        });
        assert!(approved(&sandbox, cx));
    }

    /// Starts outside its sandbox where the delegate says so, and says so.
    struct FollowsTheSettingsServer;

    impl AgentServer for FollowsTheSettingsServer {
        fn logo(&self) -> ui::IconName {
            ui::IconName::ZedAgent
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("sandboxed-stub")
        }

        fn connect(
            &self,
            delegate: AgentServerDelegate,
            _project: Entity<Project>,
            cx: &mut App,
        ) -> Task<Result<Rc<dyn AgentConnection>>> {
            let outside = delegate.starts_outside_sandbox("sandboxed-stub", cx);
            Task::ready(Ok(Rc::new(
                acp_thread::StubAgentConnection::new().with_runs_outside_sandbox(outside),
            )))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }
    }

    fn set_user_settings(json: &str, cx: &mut TestAppContext) {
        use gpui::BorrowAppContext as _;

        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(json, cx)
                    .result()
                    .expect("valid user settings");
            });
        });
        cx.run_until_parked();
    }

    fn runs_outside(entry: &Entity<AgentConnectionEntry>, cx: &mut TestAppContext) -> bool {
        entry.read_with(cx, |entry, _cx| match entry {
            AgentConnectionEntry::Connected(state) => state.connection.runs_outside_sandbox(),
            _ => panic!("the stub connects at once"),
        })
    }

    /// ADR-F49 decision 19 (8c): the shared connection a user's choice started
    /// outside the sandbox is not handed to another start once the user has
    /// turned that off — the next thread starts in the sandbox, unasked
    /// nothing — while the connection stays where the idle disconnect finds
    /// it. The connection kept in the sandbox for a thread started there is
    /// one of its own.
    #[gpui::test]
    async fn test_turning_the_choice_off_sends_the_next_start_into_the_sandbox(
        cx: &mut TestAppContext,
    ) {
        let (store, _project) = setup(cx).await;
        set_user_settings(
            r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } }, "agent_servers": { "sandboxed-stub": { "type": "custom", "command": "/bin/sandboxed-stub", "run_outside_sandbox": true } } }"#,
            cx,
        );
        let shared = store.update(cx, |store, cx| {
            store.request_connection(sandboxed_stub(), Rc::new(FollowsTheSettingsServer), cx)
        });
        cx.run_until_parked();
        assert!(runs_outside(&shared, cx), "the choice lets it out");
        let in_sandbox = store.update(cx, |store, cx| {
            store.request_connection_in_sandbox(
                sandboxed_stub(),
                Rc::new(FollowsTheSettingsServer),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            !runs_outside(&in_sandbox, cx),
            "kept in the sandbox whatever the settings say"
        );
        assert_ne!(in_sandbox, shared);

        // Turned off, the agent's entry kept: an agent that goes from the
        // settings takes its connections with it.
        set_user_settings(
            r#"{ "agent_servers": { "sandboxed-stub": { "type": "custom", "command": "/bin/sandboxed-stub", "run_outside_sandbox": false } } }"#,
            cx,
        );
        let next = store.update(cx, |store, cx| {
            store.request_connection(sandboxed_stub(), Rc::new(FollowsTheSettingsServer), cx)
        });
        cx.run_until_parked();
        assert_ne!(next, shared, "not handed the one outside");
        assert!(!runs_outside(&next, cx));
        store.read_with(cx, |store, _cx| {
            assert!(
                store
                    .outside_sandbox
                    .values()
                    .any(|(_, entry)| *entry == shared),
                "the idle disconnect can still find it"
            );
        });
        cx.executor().advance_clock(Duration::from_secs(10 * 60));
        cx.run_until_parked();
        store.read_with(cx, |store, _cx| {
            assert!(
                !store
                    .outside_sandbox
                    .values()
                    .any(|(_, entry)| *entry == shared),
                "nothing used it, so it went"
            );
        });
    }

    /// ADR-F49 decision 19 (8c): upstream's "Reload Agent" restarted the shared
    /// connection whichever connection the view was on. A view outside the
    /// sandbox, or in it while the settings start the agent outside, restarts
    /// its own, and the other views keep the shared one.
    #[gpui::test]
    async fn test_reloading_restarts_the_connection_the_view_is_on(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        let shared = store.update(cx, |store, cx| {
            store.request_connection(sandboxed_stub(), Rc::new(FollowsTheSettingsServer), cx)
        });
        let outside = store.update(cx, |store, cx| {
            store.connect_outside_sandbox(sandboxed_stub(), Rc::new(FollowsTheSettingsServer), cx)
        });
        let in_sandbox = store.update(cx, |store, cx| {
            store.request_connection_in_sandbox(
                sandboxed_stub(),
                Rc::new(FollowsTheSettingsServer),
                cx,
            )
        });
        cx.run_until_parked();

        store.update(cx, |store, cx| {
            store.restart_connection_at(
                sandboxed_stub(),
                ConnectionPlace::OutsideSandbox(outside),
                Rc::new(FollowsTheSettingsServer),
                cx,
            )
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _cx| {
            assert!(
                store.outside_sandbox_connection(outside).is_none(),
                "the next start of that view makes a new one"
            );
            assert!(
                store.outside_sandbox.contains_key(&outside),
                "the idle disconnect can still find the old one"
            );
            assert_eq!(store.entry(&sandboxed_stub()), Some(&shared));
            assert_eq!(store.in_sandbox.get(&sandboxed_stub()), Some(&in_sandbox));
        });

        store.update(cx, |store, cx| {
            store.restart_connection_at(
                sandboxed_stub(),
                ConnectionPlace::InSandbox,
                Rc::new(FollowsTheSettingsServer),
                cx,
            )
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _cx| {
            assert!(!store.in_sandbox.contains_key(&sandboxed_stub()));
            assert_eq!(store.entry(&sandboxed_stub()), Some(&shared));
        });

        store.update(cx, |store, cx| {
            store.restart_connection_at(
                sandboxed_stub(),
                ConnectionPlace::AsTheSettingsSay,
                Rc::new(FollowsTheSettingsServer),
                cx,
            )
        });
        cx.run_until_parked();
        store.read_with(cx, |store, _cx| {
            let restarted = store.entry(&sandboxed_stub()).expect("started again");
            assert_ne!(restarted, &shared);
        });
    }

    /// `agent.idle_disconnect_minutes: 0` keeps every connection for as long
    /// as the window, the way this fork behaved before.
    #[gpui::test]
    async fn test_the_idle_disconnect_can_be_turned_off(cx: &mut TestAppContext) {
        let (store, _project) = setup(cx).await;
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.idle_disconnect_minutes = 0;
            AgentSettings::override_global(settings, cx);
        });
        connect_to_a_sandboxed_stub(&store, TestSandbox::new(&[1]), cx);

        cx.executor().advance_clock(Duration::from_secs(60 * 60));
        cx.run_until_parked();
        assert!(
            is_connected(&store, cx),
            "a user who turned the disconnect off keeps every connection"
        );
    }
}
