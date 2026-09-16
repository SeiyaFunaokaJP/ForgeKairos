use std::rc::Rc;
use std::time::{Duration, Instant};

use acp_thread::{AgentConnection, LoadError};
use agent_servers::AcpConnection;
use agent_servers::{AgentServer, AgentServerDelegate};
use agent_settings::AgentSettings;
use anyhow::Result;
use collections::HashMap;
use futures::{FutureExt, future::Shared};
use gpui::{
    App, AppContext, Context, Entity, EntityId, EventEmitter, SharedString, Subscription, Task,
    WeakEntity,
};

use project::{AgentServerStore, AgentServersUpdated, Project};
use settings::{Settings as _, SettingsStore};
use watch::Receiver;

use crate::Agent;
use crate::conversation_view::ConversationView;

/// ADR-F49 (new decision): how often the store asks whether its connections
/// are still used. A connection goes at most this long after its grace ran
/// out.
///
/// Closing a thread's tab is not an event this can subscribe to — the thread
/// lives on in the panel's parking slots with its connection in hand — so the
/// question is asked from here rather than answered from there.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

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
}

impl ConnectionSlot {
    fn agent(&self) -> &Agent {
        match self {
            ConnectionSlot::Shared(agent) => agent,
            ConnectionSlot::OutsideSandbox { agent, .. } => agent,
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
    next_outside_sandbox_id: u64,
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
            next_outside_sandbox_id: 0,
            new_versions: HashMap::default(),
            holders: HashMap::default(),
            leases: HashMap::default(),
            unused_since: HashMap::default(),
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
            let entry = entry.clone();
            // ADR-F49 (new decision): whoever asked is about to use it, so the
            // grace starts over. Not every caller is a view the idle check can
            // ask later (a thread import listing sessions, an archived thread
            // being deleted on the agent), and this is what keeps the
            // connection under those.
            self.unused_since.remove(&ConnectionSlot::Shared(key));
            return entry;
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
        let slot = match outside_sandbox {
            Some(id) => ConnectionSlot::OutsideSandbox {
                id,
                agent: key.clone(),
            },
            None => ConnectionSlot::Shared(key.clone()),
        };
        self.unused_since.remove(&slot);
        self.holders.retain(|_, (_, view)| view.upgrade().is_some());
        self.holders.insert(view.entity_id(), (slot, view));
        self.watch_idle_connections(cx);
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
        let slot = ConnectionSlot::Shared(key.clone());
        self.unused_since.remove(&slot);
        ConnectionLease(self.leases.entry(slot).or_default().clone())
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
            .chain(self.outside_sandbox.iter().map(|(id, (agent, _))| {
                ConnectionSlot::OutsideSandbox {
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
                    .is_some_and(|view| view.read(cx).is_using_connection(view.entity_id(), cx))
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
        self.outside_sandbox.retain(|_, (key, _)| still_there(key));
        self.unused_since
            .retain(|slot, _| still_there(slot.agent()));
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
