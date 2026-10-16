use super::*;
use futures::channel::oneshot;
use settings::AgentConfigOptionValue;

const PICKER_AGENT: &str = "Pickers";

/// What a session of [`PickerAgentServer`]'s agent offers.
#[derive(Clone)]
enum Offers {
    ConfigOptions,
    Modes,
}

fn model_option(current: &str) -> acp_v1::SessionConfigOption {
    acp_v1::SessionConfigOption::select(
        "model",
        "Model",
        current.to_string(),
        vec![
            acp_v1::SessionConfigSelectOption::new("opus", "Opus"),
            acp_v1::SessionConfigSelectOption::new("sonnet", "Sonnet"),
            acp_v1::SessionConfigSelectOption::new("haiku", "Haiku"),
        ],
    )
    .category(acp_v1::SessionConfigOptionCategory::Model)
}

fn fast_mode_option(current: bool) -> acp_v1::SessionConfigOption {
    acp_v1::SessionConfigOption::boolean("fast", "Fast mode", current)
}

fn modes(current: &str) -> acp_v1::SessionModeState {
    acp_v1::SessionModeState::new(
        current.to_string(),
        vec![
            acp_v1::SessionMode::new("default", "Default"),
            acp_v1::SessionMode::new("plan", "Plan"),
        ],
    )
}

/// What the agent was asked, in order.
type Log = Arc<Mutex<Vec<String>>>;

/// An external agent that starts on its first message, whose sessions offer
/// options or modes and announce a slash command once they have started.
struct PickerAgentServer {
    offers: Offers,
    log: Log,
    starts: Arc<AtomicUsize>,
    /// Holds the agent's answer to a change of option or mode until the test
    /// lets it go.
    hold_changes: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    default_config_options: Arc<Mutex<HashMap<String, AgentConfigOptionValue>>>,
    default_mode: Arc<Mutex<Option<acp_v1::SessionModeId>>>,
    /// ADR-F103: whether the agent's options may be read before its first
    /// thread has started.
    reads_options_before_start: bool,
    /// ADR-F103: the session that would read the options cannot be had.
    fail_reads: bool,
    /// Holds the session that reads the options until the test lets it go.
    hold_reads: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    /// The sessions that read the options and are gone again.
    read_sessions_released: Arc<AtomicUsize>,
    /// Where each start went: outside the sandbox, past the agent's own hosts.
    places: Arc<Mutex<Vec<(bool, bool)>>>,
}

impl PickerAgentServer {
    fn new(offers: Offers) -> Self {
        Self {
            offers,
            log: Log::default(),
            starts: Arc::default(),
            hold_changes: Arc::default(),
            default_config_options: Arc::default(),
            default_mode: Arc::default(),
            reads_options_before_start: false,
            fail_reads: false,
            hold_reads: Arc::default(),
            read_sessions_released: Arc::default(),
            places: Arc::default(),
        }
    }

    /// ADR-F103: an agent whose options are read before its first thread.
    fn reading_options_before_start(offers: Offers) -> Self {
        Self {
            reads_options_before_start: true,
            ..Self::new(offers)
        }
    }

    fn places(&self) -> Vec<(bool, bool)> {
        self.places.lock().clone()
    }

    fn read_sessions_released(&self) -> usize {
        self.read_sessions_released.load(Ordering::SeqCst)
    }

    fn hold_reads(&self) {
        *self.hold_reads.lock() = Some(Vec::new());
    }

    fn release_reads(&self) {
        for answer in self.hold_reads.lock().take().unwrap_or_default() {
            answer.send(()).ok();
        }
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().clone()
    }

    fn hold_changes(&self) {
        *self.hold_changes.lock() = Some(Vec::new());
    }

    fn release_changes(&self) {
        for answer in self.hold_changes.lock().take().unwrap_or_default() {
            answer.send(()).ok();
        }
    }
}

impl AgentServer for PickerAgentServer {
    fn logo(&self) -> ui::IconName {
        ui::IconName::ZedAgent
    }

    fn agent_id(&self) -> AgentId {
        PICKER_AGENT.into()
    }

    fn starts_on_first_message(&self) -> bool {
        true
    }

    fn reads_session_options_before_start(&self) -> bool {
        self.reads_options_before_start
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        _project: Entity<Project>,
        cx: &mut App,
    ) -> Task<gpui::Result<Rc<dyn AgentConnection>>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.places.lock().push((
            delegate.starts_outside_sandbox(PICKER_AGENT, cx),
            delegate.lifts_agent_hosts(cx),
        ));
        Task::ready(Ok(Rc::new(PickerConnection {
            offers: self.offers.clone(),
            log: self.log.clone(),
            hold_changes: self.hold_changes.clone(),
            sessions: Rc::default(),
            fail_reads: self.fail_reads,
            hold_reads: self.hold_reads.clone(),
            read_sessions_released: self.read_sessions_released.clone(),
        })))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }

    fn default_mode(&self, _cx: &App) -> Option<acp_v1::SessionModeId> {
        self.default_mode.lock().clone()
    }

    fn set_default_mode(
        &self,
        mode_id: Option<acp_v1::SessionModeId>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
        *self.default_mode.lock() = mode_id;
    }

    fn default_config_option(&self, config_id: &str, _cx: &App) -> Option<AgentConfigOptionValue> {
        self.default_config_options.lock().get(config_id).cloned()
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
        let mut defaults = self.default_config_options.lock();
        match value {
            Some(value) => defaults.insert(config_id.to_string(), value),
            None => defaults.remove(config_id),
        };
    }
}

/// A session's options and modes, as the agent keeps them.
struct PickerSession {
    config_options: RefCell<Vec<acp_v1::SessionConfigOption>>,
    modes: RefCell<acp_v1::SessionModeState>,
    log: Log,
    hold_changes: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
}

impl PickerSession {
    /// The agent's answer to a change, held while the test holds changes.
    fn answer(&self, cx: &mut App) -> Task<Result<()>> {
        let mut hold_changes = self.hold_changes.lock();
        let Some(held) = hold_changes.as_mut() else {
            return Task::ready(Ok(()));
        };
        let (sender, receiver) = oneshot::channel();
        held.push(sender);
        cx.background_spawn(async move {
            receiver
                .await
                .map_err(|_| anyhow!("the answer was dropped"))
        })
    }
}

impl acp_thread::AgentSessionConfigOptions for PickerSession {
    fn config_options(&self) -> Vec<acp_v1::SessionConfigOption> {
        self.config_options.borrow().clone()
    }

    fn set_config_option(
        &self,
        config_id: acp_v1::SessionConfigId,
        value: acp_v1::SessionConfigOptionValue,
        cx: &mut App,
    ) -> Task<Result<Vec<acp_v1::SessionConfigOption>>> {
        let shown = match &value {
            acp_v1::SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
            acp_v1::SessionConfigOptionValue::Boolean { value } => value.to_string(),
            _ => "?".to_string(),
        };
        self.log.lock().push(format!("set {}={shown}", config_id.0));
        for option in self.config_options.borrow_mut().iter_mut() {
            if option.id != config_id {
                continue;
            }
            match (&mut option.kind, &value) {
                (
                    acp_v1::SessionConfigKind::Select(select),
                    acp_v1::SessionConfigOptionValue::ValueId { value },
                ) => select.current_value = value.clone(),
                (
                    acp_v1::SessionConfigKind::Boolean(boolean),
                    acp_v1::SessionConfigOptionValue::Boolean { value },
                ) => boolean.current_value = *value,
                _ => {}
            }
        }
        let options = self.config_options.borrow().clone();
        let answer = self.answer(cx);
        cx.background_spawn(async move {
            answer.await?;
            Ok(options)
        })
    }
}

impl acp_thread::AgentSessionModes for PickerSession {
    fn current_mode(&self) -> acp_v1::SessionModeId {
        self.modes.borrow().current_mode_id.clone()
    }

    fn all_modes(&self) -> Vec<acp_v1::SessionMode> {
        self.modes.borrow().available_modes.clone()
    }

    fn set_mode(&self, mode: acp_v1::SessionModeId, cx: &mut App) -> Task<Result<()>> {
        self.log.lock().push(format!("mode {mode}"));
        self.modes.borrow_mut().current_mode_id = mode;
        self.answer(cx)
    }
}

struct PickerConnection {
    offers: Offers,
    log: Log,
    hold_changes: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    sessions: Rc<RefCell<HashMap<acp_v1::SessionId, Rc<PickerSession>>>>,
    fail_reads: bool,
    hold_reads: Arc<Mutex<Option<Vec<oneshot::Sender<()>>>>>,
    read_sessions_released: Arc<AtomicUsize>,
}

impl AgentConnection for PickerConnection {
    fn agent_id(&self) -> AgentId {
        PICKER_AGENT.into()
    }

    fn telemetry_id(&self) -> SharedString {
        "pickers".into()
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        _work_dirs: PathList,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<AcpThread>>> {
        let session_id =
            acp_v1::SessionId::new(format!("session-{}", self.sessions.borrow().len() + 1));
        self.sessions.borrow_mut().insert(
            session_id.clone(),
            Rc::new(PickerSession {
                config_options: RefCell::new(vec![model_option("opus"), fast_mode_option(false)]),
                modes: RefCell::new(modes("default")),
                log: self.log.clone(),
                hold_changes: self.hold_changes.clone(),
            }),
        );
        let thread = build_test_thread(self, project, "PickerConnection", session_id, cx);
        Task::ready(Ok(thread))
    }

    /// ADR-F103: a session like a thread's, which announces its slash command
    /// a while after it has started, as agents do.
    fn new_session_to_read_options(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<gpui::Result<Entity<AcpThread>>> {
        self.log.lock().push("read options".to_string());
        if self.fail_reads {
            return Task::ready(Err(anyhow!("the agent did not answer")));
        }
        let held = self.hold_reads.lock().as_mut().map(|held| {
            let (sender, receiver) = oneshot::channel();
            held.push(sender);
            receiver
        });
        cx.spawn(async move |cx| {
            if let Some(held) = held {
                held.await.ok();
            }
            let thread = cx
                .update(|cx| self.clone().new_session(project, work_dirs, cx))
                .await?;
            let released = self.read_sessions_released.clone();
            cx.update(|cx| {
                cx.observe_release(&thread, move |_, _| {
                    released.fetch_add(1, Ordering::SeqCst);
                })
                .detach();
            });
            cx.spawn({
                let thread = thread.downgrade();
                async move |cx| {
                    cx.background_executor()
                        .timer(COMMANDS_ANNOUNCED_AFTER)
                        .await;
                    thread
                        .update(cx, |thread, cx| {
                            thread
                                .handle_session_update(
                                    acp_v1::SessionUpdate::AvailableCommandsUpdate(
                                        acp_v1::AvailableCommandsUpdate::new(vec![
                                            acp_v1::AvailableCommand::new(
                                                "review",
                                                "Review the changes",
                                            ),
                                        ]),
                                    ),
                                    cx,
                                )
                                .expect("the commands are taken in");
                        })
                        .log_err();
                }
            })
            .detach();
            Ok(thread)
        })
    }

    fn session_config_options(
        &self,
        session_id: &acp_v1::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn acp_thread::AgentSessionConfigOptions>> {
        if !matches!(self.offers, Offers::ConfigOptions) {
            return None;
        }
        let session = self.sessions.borrow().get(session_id)?.clone();
        Some(session)
    }

    fn session_modes(
        &self,
        session_id: &acp_v1::SessionId,
        _cx: &App,
    ) -> Option<Rc<dyn acp_thread::AgentSessionModes>> {
        if !matches!(self.offers, Offers::Modes) {
            return None;
        }
        let session = self.sessions.borrow().get(session_id)?.clone();
        Some(session)
    }

    fn auth_methods(&self) -> &[acp_v1::AuthMethod] {
        &[]
    }

    fn authenticate(
        &self,
        _method_id: acp_v1::AuthMethodId,
        _cx: &mut App,
    ) -> Task<gpui::Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt(
        &self,
        params: acp_v1::PromptRequest,
        _cx: &mut App,
    ) -> Task<gpui::Result<acp_v1::PromptResponse>> {
        let text = params
            .prompt
            .iter()
            .filter_map(|block| match block {
                acp_v1::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<String>();
        self.log.lock().push(format!("prompt {text}"));
        Task::ready(Ok(acp_v1::PromptResponse::new(acp_v1::StopReason::EndTurn)))
    }

    fn cancel(&self, _session_id: &acp_v1::SessionId, _cx: &mut App) {}

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

/// The agent announces its slash commands once its session has started, as
/// agents do, after the session itself.
fn announce_commands(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    let thread = view
        .read_with(cx, |view, _| view.root_thread_view())
        .expect("a running thread")
        .read_with(cx, |thread_view, _| thread_view.thread.clone());
    thread.update(cx, |thread, cx| {
        thread
            .handle_session_update(
                acp_v1::SessionUpdate::AvailableCommandsUpdate(
                    acp_v1::AvailableCommandsUpdate::new(vec![acp_v1::AvailableCommand::new(
                        "review",
                        "Review the changes",
                    )]),
                ),
                cx,
            )
            .expect("the commands are taken in");
    });
    cx.run_until_parked();
}

/// A new thread of `server`'s agent in a window of its own.
async fn open_view(
    server: Rc<PickerAgentServer>,
    cx: &mut TestAppContext,
) -> (Entity<ConversationView>, &mut VisualTestContext) {
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
    let view = new_view(
        server,
        workspace.downgrade(),
        project,
        Some(thread_store),
        cx,
    );
    (view, cx)
}

/// Another new thread of `server`'s agent, beside `view` in its window, with
/// a connection store of its own as another panel's thread would have.
fn open_view_beside(
    server: Rc<PickerAgentServer>,
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Entity<ConversationView> {
    let (workspace, project, thread_store) = view.read_with(cx, |view, _| {
        (
            view.workspace.clone(),
            view.project.clone(),
            view.thread_store.clone(),
        )
    });
    new_view(server, workspace, project, thread_store, cx)
}

fn new_view(
    server: Rc<PickerAgentServer>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    thread_store: Option<Entity<ThreadStore>>,
    cx: &mut VisualTestContext,
) -> Entity<ConversationView> {
    let connection_store =
        cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
    new_view_on(
        server,
        connection_store,
        workspace,
        project,
        thread_store,
        cx,
    )
}

/// Another new thread of `server`'s agent beside `view`, in the same panel
/// (on the same connection store).
fn open_view_in_the_same_panel(
    server: Rc<PickerAgentServer>,
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Entity<ConversationView> {
    let (connection_store, workspace, project, thread_store) = view.read_with(cx, |view, _| {
        (
            view.connection_store.clone(),
            view.workspace.clone(),
            view.project.clone(),
            view.thread_store.clone(),
        )
    });
    new_view_on(
        server,
        connection_store,
        workspace,
        project,
        thread_store,
        cx,
    )
}

fn new_view_on(
    server: Rc<PickerAgentServer>,
    connection_store: Entity<AgentConnectionStore>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    thread_store: Option<Entity<ThreadStore>>,
    cx: &mut VisualTestContext,
) -> Entity<ConversationView> {
    let view = cx.update(|window, cx| {
        cx.new(|cx| {
            ConversationView::new(
                server,
                connection_store,
                Agent::Custom {
                    id: PICKER_AGENT.into(),
                },
                None,
                None,
                None,
                None,
                None,
                workspace,
                project,
                thread_store,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    add_to_workspace(view.clone(), cx);
    cx.run_until_parked();
    view
}

/// What the not yet started thread of `view` offers to pick, if anything.
fn prelaunch_options(
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Option<Rc<PrelaunchSessionOptions>> {
    view.read_with(cx, |view, _| match &view.server_state {
        ServerState::Parked(parked) => parked
            .prelaunch
            .as_ref()
            .map(|prelaunch| prelaunch.options.clone()),
        _ => None,
    })
}

fn current_value(options: &[acp_v1::SessionConfigOption], config_id: &str) -> String {
    let option = options
        .iter()
        .find(|option| &*option.id.0 == config_id)
        .expect("the option is offered");
    match &option.kind {
        acp_v1::SessionConfigKind::Select(select) => select.current_value.0.to_string(),
        acp_v1::SessionConfigKind::Boolean(boolean) => boolean.current_value.to_string(),
        _ => panic!("an option of an unknown kind"),
    }
}

/// The options of the running session of `view`.
fn session_options(
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Vec<acp_v1::SessionConfigOption> {
    let thread_view = view
        .read_with(cx, |view, _| view.root_thread_view())
        .expect("a running thread");
    thread_view.read_with(cx, |thread_view, cx| {
        let thread = thread_view.thread.read(cx);
        thread
            .connection()
            .session_config_options(thread.session_id(), cx)
            .expect("the session has options")
            .config_options()
    })
}

/// Runs a first thread of `server`'s agent to the end of its first turn, so
/// that what its session offers is known to the next thread.
async fn run_a_first_thread(
    server: Rc<PickerAgentServer>,
    cx: &mut TestAppContext,
) -> (Entity<ConversationView>, &mut VisualTestContext) {
    let (view, cx) = open_view(server, cx).await;
    assert!(
        prelaunch_options(&view, cx).is_none(),
        "the very first thread of an agent has nothing to offer before its start: what it \
         offers is not known yet"
    );
    send_first_message(&view, "first", cx);
    announce_commands(&view, cx);
    (view, cx)
}

/// ADR-F100: a new thread offers the model and options of the agent's last
/// session, and its slash commands, without starting the agent.
#[gpui::test]
async fn test_a_new_thread_offers_the_last_sessions_options_before_it_starts(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    assert_eq!(server.starts(), 1);

    let view = open_view_beside(server.clone(), &first, cx);
    assert_eq!(server.starts(), 1, "the new thread starts nothing");
    let options = prelaunch_options(&view, cx).expect("the last session's options");
    let offered = acp_thread::AgentSessionConfigOptions::config_options(options.as_ref());
    assert_eq!(current_value(&offered, "model"), "opus");
    assert_eq!(current_value(&offered, "fast"), "false");
    assert!(
        view.read_with(cx, |view, _| match &view.server_state {
            ServerState::Parked(parked) => parked
                .prelaunch
                .as_ref()
                .is_some_and(|prelaunch| prelaunch.config_options_view.is_some()
                    && prelaunch.mode_selector.is_none()),
            _ => false,
        }),
        "the options are shown on the session's own pickers, in place of the modes"
    );
    let commands = view
        .read_with(cx, |view, cx| view.draft_message_editor(cx))
        .expect("a draft box")
        .read_with(cx, |editor, _| {
            editor
                .session_capabilities()
                .read()
                .available_commands()
                .iter()
                .map(|command| command.name.clone())
                .collect::<Vec<_>>()
        });
    assert_eq!(commands, vec!["review".to_string()]);
}

/// ADR-F100: what is picked before the start is made on the session before
/// the first message reaches the agent; the message waits for the agent to
/// take it in.
#[gpui::test]
async fn test_options_picked_before_the_start_reach_the_session_before_the_first_message(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    let view = open_view_beside(server.clone(), &first, cx);
    let options = prelaunch_options(&view, cx).expect("the last session's options");
    cx.update(|_, cx| {
        acp_thread::AgentSessionConfigOptions::set_config_option(
            options.as_ref(),
            acp_v1::SessionConfigId::new("model"),
            acp_v1::SessionConfigOptionValue::value_id("sonnet"),
            cx,
        )
    })
    .await
    .expect("sonnet is offered");
    assert_eq!(server.starts(), 1, "a pick starts nothing");

    server.hold_changes();
    let log_before = server.log().len();
    send_first_message(&view, "second", cx);
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["set model=sonnet".to_string()],
        "the pick goes first, and the message waits for the agent's answer"
    );
    server.release_changes();
    cx.run_until_parked();
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["set model=sonnet".to_string(), "prompt second".to_string()],
        "only what was picked is made: the options left alone stay the agent's to set"
    );
    assert_eq!(
        current_value(&session_options(&view, cx), "model"),
        "sonnet"
    );
}

/// ADR-F100: the agent's default for an option is what a new session starts
/// with, so it is what the pickers show before the start; leaving it alone
/// asks nothing of the session.
#[gpui::test]
async fn test_the_pickers_start_from_the_agents_defaults(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    server.default_config_options.lock().insert(
        "model".into(),
        AgentConfigOptionValue::ValueId("haiku".into()),
    );
    server
        .default_config_options
        .lock()
        .insert("fast".into(), AgentConfigOptionValue::Boolean(true));
    // Not one the agent offers: the last session's value stays.
    server.default_config_options.lock().insert(
        "missing".into(),
        AgentConfigOptionValue::ValueId("anything".into()),
    );

    let view = open_view_beside(server.clone(), &first, cx);
    let options = prelaunch_options(&view, cx).expect("the last session's options");
    let offered = acp_thread::AgentSessionConfigOptions::config_options(options.as_ref());
    assert_eq!(current_value(&offered, "model"), "haiku");
    assert_eq!(current_value(&offered, "fast"), "true");

    let log_before = server.log().len();
    send_first_message(&view, "second", cx);
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["prompt second".to_string()],
        "nothing picked, nothing set here"
    );
}

/// ADR-F100: a value the session started with already, or no longer offers,
/// is not asked for again.
#[gpui::test]
async fn test_a_pick_the_session_already_has_or_does_not_offer_is_left_out(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    let view = open_view_beside(server.clone(), &first, cx);
    let options = prelaunch_options(&view, cx).expect("the last session's options");
    // The value the session starts with anyway.
    cx.update(|_, cx| {
        acp_thread::AgentSessionConfigOptions::set_config_option(
            options.as_ref(),
            acp_v1::SessionConfigId::new("model"),
            acp_v1::SessionConfigOptionValue::value_id("opus"),
            cx,
        )
    })
    .await
    .expect("opus is offered");
    assert!(
        cx.update(|_, cx| {
            acp_thread::AgentSessionConfigOptions::set_config_option(
                options.as_ref(),
                acp_v1::SessionConfigId::new("model"),
                acp_v1::SessionConfigOptionValue::value_id("gpt"),
                cx,
            )
        })
        .await
        .is_err(),
        "a value the last session did not offer cannot be picked"
    );
    cx.update(|_, cx| {
        acp_thread::AgentSessionConfigOptions::set_config_option(
            options.as_ref(),
            acp_v1::SessionConfigId::new("fast"),
            acp_v1::SessionConfigOptionValue::boolean(true),
            cx,
        )
    })
    .await
    .expect("fast mode is offered");

    let log_before = server.log().len();
    send_first_message(&view, "second", cx);
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["set fast=true".to_string(), "prompt second".to_string()],
    );
}

/// ADR-F100: an agent with modes and no options offers its modes before the
/// start, and the mode picked is the one the first message goes in.
#[gpui::test]
async fn test_a_mode_picked_before_the_start_reaches_the_session_first(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::Modes));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    let view = open_view_beside(server.clone(), &first, cx);
    assert!(
        view.read_with(cx, |view, _| match &view.server_state {
            ServerState::Parked(parked) => parked
                .prelaunch
                .as_ref()
                .is_some_and(|prelaunch| prelaunch.mode_selector.is_some()
                    && prelaunch.config_options_view.is_none()),
            _ => false,
        }),
        "the modes are shown on the session's own picker"
    );
    let options = prelaunch_options(&view, cx).expect("the last session's modes");
    cx.update(|_, cx| {
        acp_thread::AgentSessionModes::set_mode(
            options.as_ref(),
            acp_v1::SessionModeId::new("plan"),
            cx,
        )
    })
    .await
    .expect("plan is offered");

    server.hold_changes();
    let log_before = server.log().len();
    send_first_message(&view, "second", cx);
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["mode plan".to_string()]
    );
    server.release_changes();
    cx.run_until_parked();
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["mode plan".to_string(), "prompt second".to_string()]
    );
}

/// ADR-F100: a first message that is one of the last session's slash
/// commands goes to the agent as that command, although the new session has
/// not announced its own commands yet.
#[gpui::test]
async fn test_a_first_message_can_be_a_slash_command_of_the_last_session(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (first, cx) = run_a_first_thread(server.clone(), cx).await;
    let view = open_view_beside(server.clone(), &first, cx);
    let log_before = server.log().len();
    send_first_message(&view, "/review", cx);
    assert_eq!(
        server.log()[log_before..].to_vec(),
        vec!["prompt /review".to_string()],
        "the command is not refused as unknown"
    );
}

/// ADR-F100: what is offered before the start is what the agent's sessions
/// offered when they started, not what one changed to later: a mode the agent
/// left on its own is not the next thread's.
#[gpui::test]
async fn test_the_next_thread_offers_what_the_last_session_started_with(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::Modes));
    let (view, cx) = open_view(server.clone(), cx).await;
    send_first_message(&view, "first", cx);
    let thread_view = view
        .read_with(cx, |view, _| view.root_thread_view())
        .expect("a running thread");
    thread_view.update(cx, |thread_view, cx| {
        let thread = thread_view.thread.read(cx);
        thread
            .connection()
            .session_modes(thread.session_id(), cx)
            .expect("the session has modes")
            .set_mode(acp_v1::SessionModeId::new("plan"), cx)
            .detach();
    });
    announce_commands(&view, cx);

    let view = open_view_beside(server, &view, cx);
    let options = prelaunch_options(&view, cx).expect("the last session's modes");
    assert_eq!(
        acp_thread::AgentSessionModes::current_mode(options.as_ref()),
        acp_v1::SessionModeId::new("default")
    );
}

/// ADR-F103: how long after its session the agent announces its slash
/// commands.
const COMMANDS_ANNOUNCED_AFTER: std::time::Duration = std::time::Duration::from_millis(500);

/// ADR-F103: the agent announces the slash commands of the session that reads
/// its options, which the read waits for.
fn let_the_agent_announce_its_commands(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(COMMANDS_ANNOUNCED_AFTER);
    cx.run_until_parked();
}

fn picker_agent() -> Agent {
    Agent::Custom {
        id: PICKER_AGENT.into(),
    }
}

/// Whether the store of `view` has a connection to the agent.
fn agent_is_connected(view: &Entity<ConversationView>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |view, cx| {
        view.connection_store
            .read(cx)
            .connection_status(&picker_agent(), cx)
            != crate::agent_connection_store::AgentConnectionStatus::Disconnected
    })
}

fn draws(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    view.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
}

/// ADR-F103: the Add Context button of a not yet started thread opens the
/// menu a running thread's does, and what it inserts goes into the draft box.
#[gpui::test]
async fn test_add_context_on_a_not_yet_started_thread_acts_on_its_draft(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (view, cx) = open_view(server.clone(), cx).await;
    draws(&view, cx);
    let editor = view
        .read_with(cx, |view, cx| view.draft_message_editor(cx))
        .expect("a draft box");
    editor.update_in(cx, |editor, window, cx| {
        editor.focus_handle(cx).focus(window, cx);
    });
    cx.dispatch_action(OpenAddContextMenu);
    cx.run_until_parked();
    assert!(
        view.read_with(cx, |view, _| view
            .prelaunch_add_context_menu_handle
            .is_deployed()),
        "the Add Context menu is open on the row of the not yet started thread"
    );
    // The menu takes the focus two frames after it is shown.
    for _ in 0..2 {
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
    }
    cx.dispatch_action(menu::SelectFirst);
    cx.dispatch_action(menu::Confirm);
    cx.run_until_parked();
    let text = editor.read_with(cx, |editor, cx| editor.text(cx));
    assert!(
        text.starts_with("@file"),
        "Files & Directories starts a file mention in the draft box: {text:?}"
    );
    assert_eq!(server.starts(), 0, "nothing of it starts the agent");
}

/// ADR-F103: following picked on a not yet started thread's row is on once
/// its first message has started the agent.
#[gpui::test]
async fn test_following_picked_before_the_start_is_on_once_it_starts(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::new(Offers::ConfigOptions));
    let (view, cx) = open_view(server.clone(), cx).await;
    draws(&view, cx);
    let follow = cx
        .debug_bounds("ICON-Crosshair")
        .expect("the row of the not yet started thread has the follow button");
    cx.simulate_click(follow.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.follow_on_start));
    assert_eq!(server.starts(), 0, "picking it starts nothing");

    send_first_message(&view, "first", cx);
    let thread_view = view
        .read_with(cx, |view, _| view.root_thread_view())
        .expect("a running thread");
    assert!(
        thread_view.read_with(cx, |thread_view, _| thread_view.should_be_following),
        "the thread follows the agent from its first message"
    );
}

/// ADR-F103: an agent no thread of which has started yet has its options read
/// once, in a session of its own that is given nothing to do and is closed
/// again, so that the new thread offers them before its first message.
#[gpui::test]
async fn test_the_options_of_an_agent_with_no_session_yet_are_read_once(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::reading_options_before_start(
        Offers::ConfigOptions,
    ));
    server.hold_reads();
    let (view, cx) = open_view(server.clone(), cx).await;
    assert!(
        matches!(
            view.read_with(cx, |view, _| match &view.prelaunch_options_read {
                PrelaunchOptionsRead::Reading { .. } => Some(()),
                _ => None,
            }),
            Some(())
        ),
        "the options are being read"
    );
    let beside = open_view_in_the_same_panel(server.clone(), &view, cx);
    assert_eq!(
        server.log(),
        vec!["read options".to_string()],
        "one read at a time for the agent"
    );
    server.release_reads();
    cx.run_until_parked();
    let_the_agent_announce_its_commands(cx);

    assert_eq!(server.starts(), 1);
    assert_eq!(
        server.log(),
        vec!["read options".to_string()],
        "nothing is sent and nothing is set in the session that reads the options"
    );
    let snapshot = cx
        .update(|_, cx| {
            crate::thread_metadata_store::last_session_options(&PICKER_AGENT.into(), cx)
        })
        .expect("what the agent offers is written down");
    assert!(snapshot.read_before_start);
    assert_eq!(
        snapshot
            .available_commands
            .iter()
            .map(|command| command.name.clone())
            .collect::<Vec<_>>(),
        vec!["review".to_string()],
        "the slash commands the session announced after it started are waited for"
    );
    for view in [&view, &beside] {
        let options = prelaunch_options(view, cx).expect("the read options are offered");
        let offered = acp_thread::AgentSessionConfigOptions::config_options(options.as_ref());
        assert_eq!(current_value(&offered, "model"), "opus");
    }
    assert_eq!(server.read_sessions_released(), 1, "the session is closed");
    assert!(
        !agent_is_connected(&view, cx),
        "the agent that only read its options does not keep running"
    );

    let another = open_view_beside(server.clone(), &view, cx);
    assert_eq!(
        server.starts(),
        1,
        "read once: the next thread offers what was read"
    );
    assert!(prelaunch_options(&another, cx).is_some());
}

/// ADR-F103: the options are read in the sandbox, keeping to the agent's own
/// hosts, even where the user's settings start the agent outside it.
#[gpui::test]
async fn test_the_options_are_read_in_the_sandbox_whatever_the_settings(cx: &mut TestAppContext) {
    use gpui::BorrowAppContext as _;

    init_test(cx);
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true, "allow_unrestricted_network": true } }, "agent_servers": { "Pickers": { "type": "registry", "run_outside_sandbox": true } } }"#,
                    cx,
                )
                .result()
                .expect("valid user settings");
        });
    });
    let server = Rc::new(PickerAgentServer::reading_options_before_start(
        Offers::ConfigOptions,
    ));
    let (view, cx) = open_view(server.clone(), cx).await;
    let_the_agent_announce_its_commands(cx);
    assert!(prelaunch_options(&view, cx).is_some());
    assert_eq!(
        server.places(),
        vec![(false, false)],
        "in the sandbox, with the agent's own hosts only"
    );
}

/// ADR-F103: a first message sent while the options are still being read
/// starts the thread the usual way, on the connection already coming up,
/// which the read then leaves to it.
#[gpui::test]
async fn test_a_message_sent_while_the_options_are_read_goes_the_usual_way(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer::reading_options_before_start(
        Offers::ConfigOptions,
    ));
    server.hold_reads();
    let (view, cx) = open_view(server.clone(), cx).await;
    send_first_message(&view, "hello", cx);
    assert_eq!(
        server.log(),
        vec!["read options".to_string(), "prompt hello".to_string()],
        "the message does not wait for the read"
    );
    assert!(view.read_with(cx, |view, _| view.root_thread_view().is_some()));

    server.release_reads();
    cx.run_until_parked();
    let_the_agent_announce_its_commands(cx);
    assert_eq!(server.starts(), 1, "one agent for both");
    assert_eq!(server.read_sessions_released(), 1);
    assert!(
        agent_is_connected(&view, cx),
        "the thread keeps the connection the read was on"
    );
    assert!(
        !cx.update(|_, cx| {
            crate::thread_metadata_store::last_session_options(&PICKER_AGENT.into(), cx)
        })
        .expect("the thread's session wrote what it offers")
        .read_before_start,
        "what the thread's own session offers is not written over by the read"
    );
    assert!(
        view.read_with(cx, |view, _| view.root_thread_view().is_some()),
        "the thread is still running"
    );
}

/// ADR-F103: options that could not be read are not asked for again in the
/// same panel, and the row says they appear once the agent starts.
#[gpui::test]
async fn test_options_that_could_not_be_read_are_not_read_again(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(PickerAgentServer {
        fail_reads: true,
        ..PickerAgentServer::reading_options_before_start(Offers::ConfigOptions)
    });
    let (view, cx) = open_view(server.clone(), cx).await;
    let failed = |view: &Entity<ConversationView>, cx: &mut VisualTestContext| {
        view.read_with(cx, |view, _| {
            matches!(view.prelaunch_options_read, PrelaunchOptionsRead::Failed)
        })
    };
    assert!(
        failed(&view, cx),
        "the row says the options could not be read"
    );
    assert!(prelaunch_options(&view, cx).is_none());
    assert!(
        !agent_is_connected(&view, cx),
        "the agent started for the read does not keep running"
    );

    let beside = open_view_in_the_same_panel(server.clone(), &view, cx);
    assert!(failed(&beside, cx));
    assert_eq!(
        server.log(),
        vec!["read options".to_string()],
        "not tried again in the same panel"
    );

    let in_another_panel = open_view_beside(server.clone(), &view, cx);
    assert!(failed(&in_another_panel, cx));
    assert_eq!(
        server.log(),
        vec!["read options".to_string(), "read options".to_string()],
        "a panel opened again tries once more"
    );
}
