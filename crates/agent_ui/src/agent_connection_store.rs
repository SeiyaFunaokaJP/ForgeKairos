use std::rc::Rc;

use acp_thread::{AgentConnection, LoadError};
use agent_servers::AcpConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{App, AppContext, Context, Entity, EventEmitter, SharedString, Subscription, Task};

use project::{AgentServerStore, AgentServersUpdated, Project};
use watch::Receiver;

use crate::Agent;

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

#[derive(Clone)]
enum ConnectionSlot {
    Shared(Agent),
    OutsideSandbox {
        id: OutsideSandboxConnectionId,
        agent: Agent,
    },
}

impl ConnectionSlot {
    fn agent(&self) -> &Agent {
        match self {
            ConnectionSlot::Shared(agent) => agent,
            ConnectionSlot::OutsideSandbox { agent, .. } => agent,
        }
    }
}

pub struct AgentConnectionStore {
    project: Entity<Project>,
    entries: HashMap<Agent, Entity<AgentConnectionEntry>>,
    /// Connections started outside their sandbox, by the view that confirmed
    /// each of them.
    outside_sandbox: HashMap<OutsideSandboxConnectionId, (Agent, Entity<AgentConnectionEntry>)>,
    next_outside_sandbox_id: u64,
    /// Versions reported by [`AgentConnectionEntryEvent::NewVersionAvailable`],
    /// kept after the event has been delivered. The event fires once and drops
    /// the connection entry with it, so without this the agent registry page
    /// has nothing to render a "update available" badge from (F2-5 follow-up).
    new_versions: HashMap<Agent, SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl AgentConnectionStore {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let agent_server_store = project.read(cx).agent_server_store().clone();
        let subscription = cx.subscribe(&agent_server_store, Self::handle_agent_servers_updated);
        Self {
            project,
            entries: HashMap::default(),
            outside_sandbox: HashMap::default(),
            next_outside_sandbox_id: 0,
            new_versions: HashMap::default(),
            _subscriptions: vec![subscription],
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

    pub fn request_connection(
        &mut self,
        key: Agent,
        server: Rc<dyn AgentServer>,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        if let Some(entry) = self.entries.get(&key) {
            return entry.clone();
        }
        self.insert_connection(ConnectionSlot::Shared(key), server, false, cx)
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
            true,
            cx,
        );
        id
    }

    /// The connection `id` names, while it is still there: it is dropped when
    /// it fails to start or the agent goes away, and the view then connects
    /// in the sandbox again (and has to confirm another start outside it).
    pub fn outside_sandbox_connection(
        &self,
        id: OutsideSandboxConnectionId,
    ) -> Option<Entity<AgentConnectionEntry>> {
        self.outside_sandbox
            .get(&id)
            .map(|(_, entry)| entry.clone())
    }

    fn slot_entry(&self, slot: &ConnectionSlot) -> Option<&Entity<AgentConnectionEntry>> {
        match slot {
            ConnectionSlot::Shared(key) => self.entries.get(key),
            ConnectionSlot::OutsideSandbox { id, .. } => {
                self.outside_sandbox.get(id).map(|(_, entry)| entry)
            }
        }
    }

    fn insert_slot_entry(&mut self, slot: &ConnectionSlot, entry: Entity<AgentConnectionEntry>) {
        match slot {
            ConnectionSlot::Shared(key) => {
                self.entries.insert(key.clone(), entry);
            }
            ConnectionSlot::OutsideSandbox { id, agent } => {
                self.outside_sandbox.insert(*id, (agent.clone(), entry));
            }
        }
    }

    fn remove_slot_entry(&mut self, slot: &ConnectionSlot) {
        match slot {
            ConnectionSlot::Shared(key) => {
                self.entries.remove(key);
            }
            ConnectionSlot::OutsideSandbox { id, .. } => {
                self.outside_sandbox.remove(id);
            }
        }
    }

    fn insert_connection(
        &mut self,
        slot: ConnectionSlot,
        server: Rc<dyn AgentServer>,
        run_outside_sandbox: bool,
        cx: &mut Context<Self>,
    ) -> Entity<AgentConnectionEntry> {
        let (mut new_version_rx, mut loading_status_rx, connect_task) =
            self.start_connection(server, run_outside_sandbox, cx);
        let connect_task = connect_task.shared();

        let entry = cx.new(|_cx| AgentConnectionEntry::Connecting {
            connect_task: connect_task.clone(),
        });

        self.insert_slot_entry(&slot, entry.clone());
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

                        let version = SharedString::from(version);
                        this.new_versions
                            .insert(slot.agent().clone(), version.clone());
                        entry
                            .update(cx, move |_entry, cx| {
                                cx.emit(AgentConnectionEntryEvent::NewVersionAvailable(version));
                            })
                            .ok();
                        this.remove_slot_entry(&slot);
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
        self.outside_sandbox.retain(|_, (key, _)| still_there(key));
        cx.notify();
    }

    fn start_connection(
        &self,
        server: Rc<dyn AgentServer>,
        run_outside_sandbox: bool,
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
        if run_outside_sandbox {
            delegate = delegate.outside_sandbox_once();
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
