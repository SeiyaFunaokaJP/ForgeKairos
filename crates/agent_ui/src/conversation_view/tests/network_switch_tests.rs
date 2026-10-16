use super::*;

/// The thread view of `view`, where the chat input's switches are.
fn thread_view_of(
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Entity<ThreadView> {
    view.read_with(cx, |view, _| view.root_thread_view())
        .expect("a thread view")
}

fn unrestricted_network_of(
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Option<UnrestrictedNetworkConnectionId> {
    view.read_with(cx, |view, _| view.unrestricted_network)
}

/// Whether the store still has the connection without the agent's own hosts `id`.
fn still_running(
    view: &Entity<ConversationView>,
    id: UnrestrictedNetworkConnectionId,
    cx: &mut VisualTestContext,
) -> bool {
    view.read_with(cx, |view, cx| {
        view.connection_store
            .read(cx)
            .unrestricted_network_connection(id)
            .is_some()
    })
}

fn root_session_of(
    view: &Entity<ConversationView>,
    cx: &mut VisualTestContext,
) -> Option<acp_v1::SessionId> {
    view.read_with(cx, |view, _| view.root_session_id.clone())
}

/// ADR-F99: starts a new thread's agent with its own hosts lifted, without
/// sending anything.
fn start_lifted(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    pick_lifted_network(view, cx);
    start_without_sending(view, cx);
}

/// ADR-F49 決定 19 (8c): starts a thread's agent again outside the sandbox
/// from its menu, which leaves the connection it was on.
fn restart_outside(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    view.update_in(cx, |view, window, cx| {
        view.restart_outside_sandbox(window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer(LAUNCH_FOR_THIS_THREAD_LABEL);
    cx.run_until_parked();
}

/// Lets the turn-on's wait see the settings it wrote, as the settings file's watcher would.
fn settings_take_it_in(cx: &mut VisualTestContext) {
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
}

/// The user's settings file, which the switches' one question writes the setting it turns on to.
async fn settings_file_of(view: &Entity<ConversationView>, cx: &mut VisualTestContext) -> String {
    let fs = view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
    fs.load(::paths::settings_file()).await.unwrap_or_default()
}

async fn make_the_settings_folder(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    let fs = view.read_with(cx, |view, cx| view.project.read(cx).fs().clone());
    fs.create_dir(
        ::paths::settings_file()
            .parent()
            .expect("the settings file has a parent"),
    )
    .await
    .expect("the settings folder can be made");
}

/// `COUNT` views of `server`'s agent on one project, sharing one connection
/// store, in the workspace (`in_workspace`) or held by the test alone, which
/// can then drop them.
async fn open_views<const COUNT: usize>(
    server: Rc<OutsideCapableAgentServer>,
    in_workspace: bool,
    cx: &mut TestAppContext,
) -> ([Entity<ConversationView>; COUNT], &mut VisualTestContext) {
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
    let connection_store =
        cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
    let mut views = Vec::new();
    for _ in 0..COUNT {
        let view = cx.update(|window, cx| {
            cx.new(|cx| {
                ConversationView::new(
                    server.clone(),
                    connection_store.clone(),
                    Agent::Custom {
                        id: OUTSIDE_CAPABLE_AGENT.into(),
                    },
                    None,
                    None,
                    None,
                    None,
                    None,
                    workspace.downgrade(),
                    project.clone(),
                    Some(thread_store.clone()),
                    AgentThreadSource::AgentPanel,
                    window,
                    cx,
                )
            })
        });
        cx.run_until_parked();
        if in_workspace {
            add_to_workspace(view.clone(), cx);
        }
        cx.run_until_parked();
        views.push(view);
    }
    let views = views.try_into().unwrap_or_else(|_| panic!("{COUNT} views"));
    (views, cx)
}

fn waiting_server() -> Rc<OutsideCapableAgentServer> {
    Rc::new(OutsideCapableAgentServer::new(None).starting_on_first_message())
}

/// The user's settings with running outside the sandbox and lifting the
/// network restriction allowed or not, each on its own.
fn allow(outside_sandbox: bool, unrestricted_network: bool, cx: &mut TestAppContext) {
    use gpui::BorrowAppContext as _;
    let settings = serde_json::json!({
        "agent": {
            "sandbox_permissions": {
                "allow_outside_sandbox": outside_sandbox,
                "allow_unrestricted_network": unrestricted_network,
            }
        }
    })
    .to_string();
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(&settings, cx)
                .result()
                .expect("valid user settings");
        });
    });
}

/// ADR-F99: a new thread of an agent that starts on its first message has no
/// agent behind it, nor a connection asked for, until that message is sent;
/// the send starts the agent, and the message reaches it.
#[gpui::test]
async fn test_a_new_thread_starts_its_agent_with_its_first_message(cx: &mut TestAppContext) {
    init_test(cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    assert_eq!(server.starts(), (0, 0), "nothing starts with the thread");
    assert!(view.read_with(cx, |view, _| view.is_parked()));
    assert!(
        view.read_with(cx, |view, cx| view
            .connection_store
            .read(cx)
            .entry(&view.connection_key)
            .is_none()),
        "nor is a connection asked for"
    );

    send_first_message(&view, "Hello there", cx);
    assert_eq!(server.starts(), (1, 0));
    assert!(is_connected(&view, cx));
    let thread_view = thread_view_of(&view, cx);
    thread_view.read_with(cx, |thread_view, cx| {
        let thread = thread_view.thread.read(cx);
        assert!(
            thread.to_markdown(cx).contains("Hello there"),
            "{}",
            thread.to_markdown(cx)
        );
        assert_eq!(
            thread.status(),
            ThreadStatus::Generating,
            "the agent was sent it"
        );
    });
}

/// ADR-F99: what a new thread's switches show when its first message is sent
/// is what its agent starts with — on a connection of its own with its hosts
/// lifted, or outside the sandbox — and from then on there is nothing left to
/// pick.
#[gpui::test]
async fn test_a_new_thread_starts_with_what_its_switches_show(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([lifted, outside], cx) = open_views(server.clone(), true, cx).await;
    assert_eq!(
        pending_launch_choice_of(&lifted, cx),
        Some(LaunchChoice {
            sandboxed: true,
            network_lifted: false,
        })
    );

    pick_lifted_network(&lifted, cx);
    assert_eq!(
        pending_launch_choice_of(&lifted, cx),
        Some(LaunchChoice {
            sandboxed: true,
            network_lifted: true,
        })
    );
    assert_eq!(server.starts(), (0, 0), "picking starts nothing");
    send_first_message(&lifted, "Hello", cx);
    assert_eq!(lifted_starts(&server), 1);
    assert_eq!(
        sandbox_network_of(&lifted, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
    let id = unrestricted_network_of(&lifted, cx).expect("a connection of its own");
    assert!(still_running(&lifted, id, cx));
    assert_eq!(
        pending_launch_choice_of(&lifted, cx),
        None,
        "nothing left to pick"
    );

    pick_outside_sandbox(&outside, cx);
    assert_eq!(
        pending_launch_choice_of(&outside, cx),
        Some(LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        })
    );
    assert_eq!(server.starts(), (1, 0), "picking starts nothing");
    send_first_message(&outside, "Hello", cx);
    assert_eq!(server.starts(), (1, 1));
    assert!(outside.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
    assert_eq!(pending_launch_choice_of(&outside, cx), None);
}

/// ADR-F99: clicking a new thread's switches leaves nothing behind; what a
/// thread's agent was started with is what that agent's next new thread
/// offers, and the project's default goes before it.
#[gpui::test]
async fn test_a_new_thread_offers_what_the_last_one_was_started_with(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        language_model::LanguageModelRegistry::test(cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(util::path!("/project"), json!({ "a.txt": "" }))
        .await;
    let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
    let keys =
        cx.update(|cx| project::agent_sandbox::project_thread_defaults_keys(project.read(cx), cx));
    assert_eq!(keys.len(), 1);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let agent_id = server.agent_id();
    let last_choice = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| crate::thread_metadata_store::last_launch_choice(&agent_id, cx))
    };
    let restricted = LaunchChoice {
        sandboxed: true,
        network_lifted: false,
    };
    let lifted = LaunchChoice {
        sandboxed: true,
        network_lifted: true,
    };

    let (clicked, cx) = open_outside_capable_view_on(server.clone(), None, None, project, cx).await;
    pick_lifted_network(&clicked, cx);
    pick_outside_sandbox(&clicked, cx);
    assert_eq!(last_choice(cx), None, "clicking leaves nothing behind");
    let next = open_outside_capable_view_beside(server.clone(), &clicked, cx);
    assert_eq!(pending_launch_choice_of(&next, cx), Some(restricted));

    pick_lifted_network(&next, cx);
    send_first_message(&next, "Hello", cx);
    assert_eq!(last_choice(cx), Some(lifted));
    let after = open_outside_capable_view_beside(server.clone(), &clicked, cx);
    assert_eq!(
        pending_launch_choice_of(&after, cx),
        Some(lifted),
        "what the last thread was started with"
    );

    allow_lifting_the_network(
        true,
        serde_json::json!({
            "project_thread_defaults": { keys[0].clone(): { "network": true } }
        }),
        cx,
    );
    let with_default = open_outside_capable_view_beside(server.clone(), &clicked, cx);
    assert_eq!(
        pending_launch_choice_of(&with_default, cx),
        Some(restricted),
        "the project's default goes first"
    );
}

/// ADR-F99: a thread whose agent was started with its hosts lifted is read
/// back from the history lifted, without asking; one started restricted is
/// read back restricted.
#[gpui::test]
async fn test_a_thread_started_lifted_is_resumed_lifted(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let lifted_thread = ThreadId::new();
    let (lifted, cx) =
        open_outside_capable_view(server.clone(), Some(lifted_thread), None, cx).await;
    pick_lifted_network(&lifted, cx);
    send_first_message(&lifted, "Hello", cx);
    let lifted_session = root_session_of(&lifted, cx).expect("a session");
    assert!(cx.update(|_, cx| thread_started_with_network_lifted(lifted_thread, cx)));

    let restricted_thread = ThreadId::new();
    let (restricted, cx) =
        open_outside_capable_view(server.clone(), Some(restricted_thread), None, cx).await;
    restricted.update_in(cx, |view, window, cx| {
        view.pick_launch_network(false, window, cx)
    });
    send_first_message(&restricted, "Hello", cx);
    let restricted_session = root_session_of(&restricted, cx).expect("a session");
    assert_eq!(
        sandbox_network_of(&restricted, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(!cx.update(|_, cx| thread_started_with_network_lifted(restricted_thread, cx)));
    assert_eq!(lifted_starts(&server), 1);

    let (resumed, cx) = open_outside_capable_view(
        server.clone(),
        Some(lifted_thread),
        Some(lifted_session.clone()),
        cx,
    )
    .await;
    assert!(!cx.has_pending_prompt(), "nothing is asked");
    assert_eq!(lifted_starts(&server), 2);
    assert_eq!(
        sandbox_network_of(&resumed, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
    assert_eq!(
        root_session_of(&resumed, cx),
        Some(lifted_session),
        "the conversation is read back"
    );

    let (resumed_restricted, cx) = open_outside_capable_view(
        server.clone(),
        Some(restricted_thread),
        Some(restricted_session),
        cx,
    )
    .await;
    assert_eq!(lifted_starts(&server), 2);
    assert_eq!(
        sandbox_network_of(&resumed_restricted, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
}

/// ADR-F99: starting a draft's agent again (a retry here) starts a new
/// session rather than asking the agent to read back one it never saved —
/// Claude answers "Resource not found" for that — and the text written in the
/// draft stays.
#[gpui::test]
async fn test_retrying_a_draft_starts_a_new_session_and_keeps_its_text(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    assert!(is_connected(&view, cx), "this agent starts with its thread");
    let draft_session = root_session_of(&view, cx).expect("a session");
    message_editor(&view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("Not sent yet", window, cx)
    });

    view.update_in(cx, |view, window, cx| view.retry_connection(window, cx));
    cx.run_until_parked();
    assert!(is_connected(&view, cx));
    let session = root_session_of(&view, cx);
    assert!(session.is_some());
    assert_ne!(session, Some(draft_session), "a new session");
    assert_eq!(
        message_editor(&view, cx).read_with(cx, |editor, cx| editor.text(cx)),
        "Not sent yet"
    );
}

/// ADR-F99: a choice is had only as far as the settings let it be: outside the
/// sandbox only while running outside it is allowed, lifted only while lifting
/// is allowed, and never lifted outside the sandbox, where nothing is
/// restricted.
#[gpui::test]
async fn test_a_launch_choice_is_had_only_as_far_as_the_settings_allow(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let choice = |sandboxed, network_lifted| LaunchChoice {
        sandboxed,
        network_lifted,
    };
    let effective = |picked: LaunchChoice, cx: &mut TestAppContext| {
        cx.update(|cx| ConversationView::effective_launch_choice(picked, &project, cx))
    };

    allow(true, true, cx);
    assert_eq!(effective(choice(true, true), cx), choice(true, true));
    assert_eq!(effective(choice(false, false), cx), choice(false, false));
    assert_eq!(
        effective(choice(false, true), cx),
        choice(false, false),
        "outside the sandbox there is nothing to lift"
    );

    allow(false, true, cx);
    assert_eq!(
        effective(choice(false, false), cx),
        choice(true, false),
        "in the sandbox while running outside it is not allowed"
    );
    assert_eq!(effective(choice(false, true), cx), choice(true, true));

    allow(true, false, cx);
    assert_eq!(
        effective(choice(true, true), cx),
        choice(true, false),
        "restricted while lifting is not allowed"
    );
    assert_eq!(effective(choice(false, false), cx), choice(false, false));
}

/// ADR-F96, ADR-F99: a new thread picked to start with its agent's hosts
/// lifted starts on a connection of its own; a thread that was not goes on
/// the connection the threads share, which keeps the agent's hosts.
#[gpui::test]
async fn test_a_thread_started_lifted_runs_on_a_connection_of_its_own(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([lifted, restricted], cx) = open_views(server.clone(), true, cx).await;
    pick_lifted_network(&lifted, cx);
    send_first_message(&lifted, "Hello", cx);
    send_first_message(&restricted, "Hello", cx);
    assert_eq!(server.starts(), (2, 0));
    assert_eq!(lifted_starts(&server), 1);
    assert_eq!(
        sandbox_network_of(&lifted, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
    assert!(unrestricted_network_of(&lifted, cx).is_some());
    assert_eq!(
        sandbox_network_of(&restricted, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert_eq!(unrestricted_network_of(&restricted, cx), None);
    let network_of_shared = restricted.read_with(cx, |view, cx| {
        match view
            .connection_store
            .read(cx)
            .entry(&view.connection_key)
            .map(|entry| entry.read(cx))
        {
            Some(crate::agent_connection_store::AgentConnectionEntry::Connected(state)) => {
                state.connection.sandbox_network()
            }
            _ => None,
        }
    });
    assert_eq!(network_of_shared, Some(AgentSandboxNetwork::AgentHosts));
}

/// ADR-F96, ADR-F99: where `allow_unrestricted_network` is off, lifting the
/// network restriction on a new thread's switch asks once, and the question
/// also turns the setting on; the switch shows it lifted once the settings
/// have it, without a second dialog, and nothing starts before the first
/// message. Declining writes nothing.
#[gpui::test]
async fn test_lifting_an_external_agents_hosts_asks_once_where_it_is_not_allowed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(false, serde_json::json!({}), cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    make_the_settings_folder(&view, cx).await;
    let network_lifted = |cx: &mut VisualTestContext| {
        pending_launch_choice_of(&view, cx).map(|choice| choice.network_lifted)
    };

    view.update_in(cx, |view, window, cx| {
        view.pick_launch_network(true, window, cx)
    });
    cx.run_until_parked();
    let (_message, detail) = cx.pending_prompt().expect("asked");
    assert!(
        detail.contains("Allow Lifting the Network Restriction for a Thread"),
        "{detail}"
    );
    assert!(detail.contains("Settings > AI > Sandbox"), "{detail}");
    assert!(detail.contains("network allowlist"), "{detail}");
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    settings_take_it_in(cx);
    assert_eq!(network_lifted(cx), Some(false), "declining lifts nothing");
    assert!(
        !settings_file_of(&view, cx)
            .await
            .contains("allow_unrestricted_network"),
        "and writes nothing"
    );

    view.update_in(cx, |view, window, cx| {
        view.pick_launch_network(true, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Turn On and Continue");
    cx.run_until_parked();
    assert_eq!(
        network_lifted(cx),
        Some(false),
        "nothing is lifted before the settings have it"
    );
    assert!(
        settings_file_of(&view, cx)
            .await
            .contains("\"allow_unrestricted_network\": true"),
        "agreeing writes the setting"
    );
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    settings_take_it_in(cx);
    assert!(!cx.has_pending_prompt(), "nothing else is asked");
    assert_eq!(network_lifted(cx), Some(true));
    assert_eq!(server.starts(), (0, 0), "and nothing has started");

    send_first_message(&view, "Hello", cx);
    assert_eq!(lifted_starts(&server), 1);
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
}

/// ADR-F96, ADR-F99: where running outside the sandbox is off, picking
/// outside on a new thread's sandbox switch asks once — the start's own
/// question also turns the setting on — and the first message then starts the
/// agent outside.
#[gpui::test]
async fn test_letting_an_external_agent_out_asks_once_where_it_is_not_allowed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_running_outside_sandbox(false, cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    make_the_settings_folder(&view, cx).await;
    let sandboxed = |cx: &mut VisualTestContext| {
        pending_launch_choice_of(&view, cx).map(|choice| choice.sandboxed)
    };

    view.update_in(cx, |view, window, cx| {
        view.pick_launch_sandbox(false, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    settings_take_it_in(cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(sandboxed(cx), Some(true), "declining changes nothing");
    assert!(
        !settings_file_of(&view, cx)
            .await
            .contains("allow_outside_sandbox"),
        "and writes nothing"
    );

    view.update_in(cx, |view, window, cx| {
        view.pick_launch_sandbox(false, window, cx)
    });
    cx.run_until_parked();
    let (_message, detail) = cx.pending_prompt().expect("asked");
    assert!(
        detail.contains("Allow Running Outside the Sandbox"),
        "{detail}"
    );
    cx.simulate_prompt_answer("Turn On and Start Outside");
    cx.run_until_parked();
    assert_eq!(
        sandboxed(cx),
        Some(true),
        "nothing changes before the settings have it"
    );
    assert!(
        settings_file_of(&view, cx)
            .await
            .contains("\"allow_outside_sandbox\": true"),
        "agreeing writes the setting"
    );
    allow_running_outside_sandbox(true, cx);
    settings_take_it_in(cx);
    assert!(!cx.has_pending_prompt(), "nothing else is asked");
    assert_eq!(sandboxed(cx), Some(false));
    assert_eq!(server.starts(), (0, 0), "and nothing has started");

    send_first_message(&view, "Hello", cx);
    assert_eq!(server.starts(), (0, 1));
    assert!(view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
}

/// ADR-F96: threads that start with their agent's hosts lifted share one
/// connection without them, which stays for as long as one of them is on it
/// and closes once the last leaves (started again outside the sandbox here),
/// rather than being left to the idle disconnect. The next thread to lift
/// them gets a new one.
#[gpui::test]
async fn test_lifted_threads_share_a_connection_that_closes_when_the_last_leaves(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([first, second, third], cx) = open_views(server.clone(), true, cx).await;

    start_lifted(&first, cx);
    start_lifted(&second, cx);
    assert_eq!(lifted_starts(&server), 1, "one process for both threads");
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    assert_eq!(unrestricted_network_of(&second, cx), Some(id));
    for view in [&first, &second] {
        assert_eq!(
            sandbox_network_of(view, cx),
            Some(AgentSandboxNetwork::AgentHostsLifted)
        );
    }

    restart_outside(&first, cx);
    assert!(first.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
    assert_eq!(unrestricted_network_of(&first, cx), None);
    assert!(
        still_running(&first, id, cx),
        "the other thread is still on it"
    );
    assert_eq!(
        sandbox_network_of(&second, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );

    restart_outside(&second, cx);
    assert!(
        !still_running(&second, id, cx),
        "closed once no thread is on it"
    );
    assert_eq!(server.starts(), (1, 2), "and nothing else started");

    start_lifted(&third, cx);
    assert_eq!(lifted_starts(&server), 2, "a new one for the next thread");
    assert_ne!(unrestricted_network_of(&third, cx), Some(id));
}

/// ADR-F96: turning `allow_unrestricted_network` off restricts the threads whose
/// external agent runs without its own hosts then, not at their next start: they
/// start again with the agent's hosts, reading the conversation back, and the
/// connection they leave is closed.
#[gpui::test]
async fn test_turning_lifting_off_restricts_the_running_agents_at_once(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([first, second], cx) = open_views(server.clone(), true, cx).await;
    for view in [&first, &second] {
        pick_lifted_network(view, cx);
        send_first_message(view, "Hello", cx);
    }
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    let sessions = [&first, &second].map(|view| root_session_of(view, cx));

    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt(), "nothing is asked");
    for (view, session) in [&first, &second].into_iter().zip(sessions) {
        assert!(is_connected(view, cx));
        assert_eq!(
            sandbox_network_of(view, cx),
            Some(AgentSandboxNetwork::AgentHosts)
        );
        assert_eq!(unrestricted_network_of(view, cx), None);
        assert_eq!(
            root_session_of(view, cx),
            session,
            "the conversation is read back"
        );
    }
    assert!(!still_running(&first, id, cx));
    assert_eq!(lifted_starts(&server), 1);

    allow_lifting_the_network(true, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&first, cx),
        Some(AgentSandboxNetwork::AgentHosts),
        "turning it on again lifts nothing unasked"
    );
    assert_eq!(lifted_starts(&server), 1);
}

/// ADR-F96: turning `allow_unrestricted_network` off while a thread's agent is
/// still starting without its own hosts restricts it once it has started,
/// rather than leaving it lifted until its next start.
#[gpui::test]
async fn test_turning_lifting_off_during_a_start_restricts_the_agent_once_started(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;

    view.update_in(cx, |view, window, cx| {
        let key = view.connection_key.clone();
        let agent = view.agent.clone();
        let id = view.connection_store.update(cx, |store, cx| {
            store.connect_with_unrestricted_network(key, agent, cx)
        });
        view.unrestricted_network = Some(id);
        view.reset_with(ResetSession::Resume, window, cx);
    });
    assert!(
        view.read_with(cx, |view, _| view.is_loading()),
        "still starting"
    );
    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();

    assert!(is_connected(&view, cx));
    assert_eq!(lifted_starts(&server), 1, "the start went on lifted");
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHosts),
        "and was restricted once it had started"
    );
    assert_eq!(unrestricted_network_of(&view, cx), None);
}

/// ADR-F96, ADR-F99: a draft whose agent was started outside the sandbox and
/// then let go of can be picked back into the sandbox before it starts
/// again; that start closes the connection outside the sandbox it leaves,
/// rather than leaving that agent to run unsandboxed until the idle
/// disconnect.
#[gpui::test]
async fn test_going_back_into_the_sandbox_closes_the_connection_outside(cx: &mut TestAppContext) {
    init_test(cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    pick_outside_sandbox(&view, cx);
    start_without_sending(&view, cx);
    let id = view
        .read_with(cx, |view, _| view.outside_sandbox)
        .expect("started outside");
    let outside_connection = |cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| {
            view.connection_store
                .read(cx)
                .outside_sandbox_connection(id)
                .is_some()
        })
    };
    assert!(outside_connection(cx));

    assert!(view.update_in(cx, |view, window, cx| view.test_park(window, cx)));
    assert_eq!(
        pending_launch_choice_of(&view, cx),
        Some(LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        }),
        "the draft offers what it ran with"
    );
    view.update_in(cx, |view, window, cx| {
        view.pick_launch_sandbox(true, window, cx)
    });
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt(), "going back in asks nothing");
    assert!(outside_connection(cx), "nothing closes before the start");

    start_without_sending(&view, cx);
    assert!(is_connected(&view, cx));
    assert!(!view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
    assert_eq!(server.starts(), (1, 1));
    assert!(
        !outside_connection(cx),
        "closed once no thread is on it, without waiting for the idle disconnect"
    );
}

/// ADR-F96: a connection without the agent's own hosts that a newer version of
/// the agent superseded is not handed to the next thread that lifts its
/// restriction; that one gets a new connection, while the thread already on
/// the old one keeps it.
#[gpui::test]
async fn test_a_superseded_lifted_connection_is_not_shared_with_the_next_thread(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([first, second], cx) = open_views(server.clone(), true, cx).await;
    start_lifted(&first, cx);
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    let mut new_version = server
        .new_version
        .lock()
        .take()
        .expect("the lifted start was handed a sender");
    new_version
        .send(Some("2.0.0".to_string()))
        .expect("the store listens");
    cx.run_until_parked();
    assert!(
        still_running(&first, id, cx),
        "ADR-F101: a report alone replaces nothing"
    );
    // What agreeing to the update does.
    first.update(cx, |view, cx| {
        let key = view.connection_key.clone();
        let place = view.connection_place();
        view.connection_store
            .update(cx, |store, cx| store.begin_agent_update(&key, place, cx));
    });
    cx.run_until_parked();
    assert!(!still_running(&first, id, cx), "no start is handed it");

    start_lifted(&second, cx);
    assert_eq!(
        lifted_starts(&server),
        2,
        "a new connection for the next thread"
    );
    let second_id = unrestricted_network_of(&second, cx).expect("lifted");
    assert_ne!(second_id, id);
    assert_eq!(unrestricted_network_of(&first, cx), Some(id));
    assert_eq!(
        sandbox_network_of(&first, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted),
        "the thread on the old one keeps it"
    );
    drop(new_version);
}

/// ADR-F96: turning `allow_unrestricted_network` off in the middle of an answer
/// of an agent that runs without its own hosts starts it again, which stops the
/// answer, so a notice says so; an agent that was not answering starts again
/// without one.
#[gpui::test]
async fn test_turning_lifting_off_mid_answer_says_the_answer_was_stopped(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let ([idle, answering], cx) = open_views(server.clone(), true, cx).await;
    let notice =
        workspace::notifications::NotificationId::unique::<AnswerStoppedByNetworkRestriction>();
    let noticed = |cx: &mut VisualTestContext| {
        idle.read_with(cx, |view, cx| {
            view.workspace
                .upgrade()
                .expect("the workspace")
                .read(cx)
                .notification_ids()
                .contains(&notice)
        })
    };

    start_lifted(&idle, cx);
    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&idle, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(!noticed(cx), "nothing was cut off");

    allow_lifting_the_network(true, serde_json::json!({}), cx);
    cx.run_until_parked();
    pick_lifted_network(&answering, cx);
    send_first_message(&answering, "Go on", cx);
    assert_eq!(
        sandbox_network_of(&answering, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
    let thread_view = thread_view_of(&answering, cx);
    assert_eq!(
        thread_view.read_with(cx, |thread_view, cx| thread_view.thread.read(cx).status()),
        ThreadStatus::Generating
    );

    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&answering, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(noticed(cx), "the stopped answer is told");
}

/// ADR-F96: a connection without the agent's own hosts that its last thread left
/// while it was still starting closes once it has started, rather than staying
/// for an idle disconnect that is turned off here.
#[gpui::test]
async fn test_a_lifted_connection_left_while_starting_closes_once_started(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(
        true,
        serde_json::json!({ "idle_disconnect_minutes": 0 }),
        cx,
    );
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;

    let id = view.update_in(cx, |view, window, cx| {
        let key = view.connection_key.clone();
        let agent = view.agent.clone();
        let id = view.connection_store.update(cx, |store, cx| {
            store.connect_with_unrestricted_network(key, agent, cx)
        });
        view.unrestricted_network = Some(id);
        view.reset_with(ResetSession::Resume, window, cx);
        // Restricted again before that start has come up.
        view.unrestricted_network = None;
        view.kept_network_restricted = true;
        view.reset_with(ResetSession::Resume, window, cx);
        id
    });
    cx.run_until_parked();

    assert_eq!(lifted_starts(&server), 1, "the start went on");
    assert!(is_connected(&view, cx));
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(
        !still_running(&view, id, cx),
        "closed once it had started, with no thread on it"
    );
}

/// ADR-F96: a connection without the agent's own hosts goes with the last view
/// on it, rather than staying for an idle disconnect that is turned off here.
#[gpui::test]
async fn test_a_lifted_connection_goes_with_the_last_view_on_it(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(
        true,
        serde_json::json!({ "idle_disconnect_minutes": 0 }),
        cx,
    );
    let server = waiting_server();
    let ([first, second], cx) = open_views(server.clone(), false, cx).await;
    start_lifted(&first, cx);
    start_lifted(&second, cx);
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    assert_eq!(unrestricted_network_of(&second, cx), Some(id));
    let store = first.read_with(cx, |view, _| view.connection_store.clone());
    let still_there = |cx: &mut VisualTestContext| {
        store.read_with(cx, |store, _| {
            store.unrestricted_network_connection(id).is_some()
        })
    };

    let weak_first = first.downgrade();
    drop(first);
    release_dropped_entities(cx);
    cx.run_until_parked();
    assert!(!weak_first.is_upgradable(), "the first view is gone");
    assert!(still_there(cx), "the other thread is still on it");

    let weak_second = second.downgrade();
    drop(second);
    release_dropped_entities(cx);
    cx.run_until_parked();
    assert!(!weak_second.is_upgradable(), "the second view is gone");
    assert!(!still_there(cx), "closed with the last view on it");
    assert_eq!(lifted_starts(&server), 1);
}

fn draft_text_of(view: &Entity<ConversationView>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |view, cx| {
        view.draft_message_editor(cx)
            .expect("a draft box")
            .read(cx)
            .text(cx)
    })
}

fn has_shared_connection(view: &Entity<ConversationView>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |view, cx| {
        view.connection_store
            .read(cx)
            .entry(&view.connection_key)
            .is_some()
    })
}

fn remember_last_choice(choice: LaunchChoice, cx: &mut TestAppContext) {
    cx.update(|cx| {
        crate::thread_metadata_store::record_last_launch_choice(
            OUTSIDE_CAPABLE_AGENT.into(),
            choice,
            cx,
        )
    });
}

fn last_choice_of_the_agent(cx: &mut VisualTestContext) -> Option<LaunchChoice> {
    cx.update(|_, cx| {
        crate::thread_metadata_store::last_launch_choice(&OUTSIDE_CAPABLE_AGENT.into(), cx)
    })
}

/// ADR-F99: "outside" taken over from the last thread only sets the new
/// thread's switch; its first message asks before the agent starts outside.
/// A no leaves the draft as it was, unsent and with no agent behind it; a yes
/// starts it outside.
#[gpui::test]
async fn test_a_remembered_start_outside_asks_before_the_first_message_goes(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    remember_last_choice(
        LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        },
        cx,
    );
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    assert_eq!(
        pending_launch_choice_of(&view, cx),
        Some(LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        }),
        "the switch shows outside"
    );

    send_first_message(&view, "Hello", cx);
    assert!(cx.has_pending_prompt(), "the send asks first");
    assert_eq!(server.starts(), (0, 0), "nothing starts before the answer");
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(view.read_with(cx, |view, _| view.is_parked()));
    assert_eq!(draft_text_of(&view, cx), "Hello", "the text stays");
    assert_eq!(server.starts(), (0, 0));
    assert!(
        !has_shared_connection(&view, cx),
        "nor is a connection asked for"
    );

    let editor = view
        .read_with(cx, |view, cx| view.draft_message_editor(cx))
        .expect("a draft box");
    editor.update(cx, |editor, cx| editor.send(cx));
    cx.run_until_parked();
    cx.simulate_prompt_answer(LAUNCH_FOR_THIS_THREAD_LABEL);
    cx.run_until_parked();
    assert_eq!(server.starts(), (0, 1));
    assert!(view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
    let thread_view = thread_view_of(&view, cx);
    assert!(
        thread_view.read_with(cx, |thread_view, cx| thread_view
            .thread
            .read(cx)
            .to_markdown(cx)
            .contains("Hello")),
        "and the message is sent"
    );
}

/// ADR-F99: where the project's default starts new threads outside the
/// sandbox, the first message starts the agent there without asking.
#[gpui::test]
async fn test_a_project_default_outside_starts_without_asking(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        language_model::LanguageModelRegistry::test(cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(util::path!("/project"), json!({ "a.txt": "" }))
        .await;
    let project = Project::test(fs, [Path::new(util::path!("/project"))], cx).await;
    let keys =
        cx.update(|cx| project::agent_sandbox::project_thread_defaults_keys(project.read(cx), cx));
    assert_eq!(keys.len(), 1);
    allow_lifting_the_network(
        false,
        serde_json::json!({
            "project_thread_defaults": { keys[0].clone(): { "sandbox": false } }
        }),
        cx,
    );
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view_on(server.clone(), None, None, project, cx).await;
    assert_eq!(
        pending_launch_choice_of(&view, cx).map(|choice| choice.sandboxed),
        Some(false)
    );
    send_first_message(&view, "Hello", cx);
    assert!(!cx.has_pending_prompt(), "the default does not ask");
    assert_eq!(server.starts(), (0, 1));
    assert!(view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
}

/// ADR-F99: a thread whose content is sent as it opens starts at once, and
/// nobody saw its switches, so "outside" taken over from the last thread does
/// not let it out. A prompt from outside the editor starts the agent at once
/// too, so that the thread can warn about it before the user sends it.
#[gpui::test]
async fn test_a_thread_that_starts_as_it_opens_starts_in_the_sandbox(cx: &mut TestAppContext) {
    init_test(cx);
    remember_last_choice(
        LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        },
        cx,
    );
    let server = waiting_server();
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (view, cx) = open_outside_capable_view_with_content(
        server.clone(),
        None,
        None,
        Some(AgentInitialContent::ContentBlock {
            blocks: vec![acp_v1::ContentBlock::Text(acp_v1::TextContent::new(
                "Hello",
            ))],
            auto_submit: true,
        }),
        project,
        cx,
    )
    .await;
    assert!(!cx.has_pending_prompt(), "nothing is asked");
    assert_eq!(server.starts(), (1, 0), "started in the sandbox");
    assert!(is_connected(&view, cx));
    assert!(!view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));

    let external_server = waiting_server();
    let prompt =
        crate::ExternalSourcePrompt::new("Write me a script").expect("the prompt sanitizes");
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (external, cx) = open_outside_capable_view_with_content(
        external_server.clone(),
        None,
        None,
        Some(AgentInitialContent::FromExternalSource(prompt)),
        project,
        cx,
    )
    .await;
    assert!(!external.read_with(cx, |view, _| view.is_parked()));
    assert!(is_connected(&external, cx));
    assert_eq!(external_server.starts(), (1, 0));
    let thread_view = thread_view_of(&external, cx);
    thread_view.read_with(cx, |thread_view, cx| {
        assert!(thread_view.show_external_source_prompt_warning);
        assert_eq!(
            thread_view.message_editor.read(cx).text(cx),
            "Write me a script"
        );
        assert!(thread_view.thread.read(cx).entries().is_empty(), "unsent");
    });
}

/// ADR-F99: a reload of the agent or a saved API key does not start a draft's
/// agent before its first message, and the box keeps what was typed in it.
#[gpui::test]
async fn test_a_reload_does_not_start_a_drafts_agent(cx: &mut TestAppContext) {
    init_test(cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    let editor = view
        .read_with(cx, |view, cx| view.draft_message_editor(cx))
        .expect("a draft box");
    editor.update_in(cx, |editor, window, cx| {
        editor.set_text("Not sent yet", window, cx)
    });

    view.update_in(cx, |view, window, cx| view.retry_connection(window, cx));
    cx.run_until_parked();
    view.update_in(cx, |view, window, cx| view.reset(window, cx));
    cx.run_until_parked();
    assert_eq!(server.starts(), (0, 0), "nothing started");
    assert!(view.read_with(cx, |view, _| view.is_parked()));
    assert!(!has_shared_connection(&view, cx));
    assert_eq!(draft_text_of(&view, cx), "Not sent yet");
    assert!(
        pending_launch_choice_of(&view, cx).is_some(),
        "the switches can still be picked"
    );
}

/// ADR-F99: taking back the starts outside the sandbox on the settings page
/// holds for a thread that was started outside, too: a retry does not start it
/// outside again, and its card offers to resume it outside, which asks. What
/// the last threads were started with goes as well.
#[gpui::test]
async fn test_revoking_the_consents_keeps_a_thread_started_outside_from_going_out_on_a_retry(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let server = waiting_server();
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    pick_outside_sandbox(&view, cx);
    send_first_message(&view, "Hello", cx);
    assert_eq!(server.starts(), (0, 1));
    assert_eq!(
        last_choice_of_the_agent(cx),
        Some(LaunchChoice {
            sandboxed: false,
            network_lifted: false,
        })
    );

    let revoked =
        cx.update(|_, cx| crate::thread_metadata_store::revoke_outside_sandbox_consents(cx));
    revoked.await.expect("the consents are taken back");
    assert_eq!(
        last_choice_of_the_agent(cx),
        None,
        "the last choice goes too"
    );

    view.update_in(cx, |view, window, cx| view.retry_connection(window, cx));
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(server.starts(), (0, 1), "not started outside again");
    assert!(
        matches!(
            load_error_of(&view, cx),
            Some(LoadError::SessionStartedOutsideSandbox)
        ),
        "the card offers to resume it outside, which asks"
    );
}

/// ADR-F99: a thread started with its agent's hosts lifted and then
/// restricted by turning `allow_unrestricted_network` off is no longer
/// written down as lifted, so it is not read back lifted.
#[gpui::test]
async fn test_a_thread_restricted_again_is_no_longer_written_down_as_lifted(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = waiting_server();
    let thread_id = ThreadId::new();
    let (view, cx) = open_outside_capable_view(server.clone(), Some(thread_id), None, cx).await;
    pick_lifted_network(&view, cx);
    send_first_message(&view, "Hello", cx);
    assert!(cx.update(|_, cx| thread_started_with_network_lifted(thread_id, cx)));

    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(!cx.update(|_, cx| thread_started_with_network_lifted(thread_id, cx)));
}

/// ADR-F99: a start that failed is not what the agent's next new thread
/// offers; only an agent that came up is.
#[gpui::test]
async fn test_a_failed_start_is_not_remembered_as_the_last_choice(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let restricted = LaunchChoice {
        sandboxed: true,
        network_lifted: false,
    };
    remember_last_choice(restricted, cx);
    let server =
        Rc::new(OutsideCapableAgentServer::new(Some(failed_to_start)).starting_on_first_message());
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    pick_lifted_network(&view, cx);
    send_first_message(&view, "Hello", cx);
    assert_eq!(lifted_starts(&server), 1, "the start was tried");
    assert!(load_error_of(&view, cx).is_some(), "and failed");
    assert_eq!(last_choice_of_the_agent(cx), Some(restricted));
}

/// ADR-F99: a thread resumed without its history shows no messages, yet holds
/// a conversation, so a retry resumes that session rather than starting a new
/// one as for a draft.
#[gpui::test]
async fn test_a_retry_of_a_thread_resumed_without_history_resumes_it(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let connection_store =
        cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
    let session = acp_v1::SessionId::new("resume-session");
    let view = cx.update(|window, cx| {
        cx.new(|cx| {
            ConversationView::new(
                Rc::new(StubAgentServer::new(ResumeOnlyAgentConnection)),
                connection_store,
                Agent::Custom { id: "Test".into() },
                Some(session.clone()),
                None,
                None,
                None,
                None,
                workspace.downgrade(),
                project,
                None,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            )
        })
    });
    cx.run_until_parked();
    let thread_view = thread_view_of(&view, cx);
    assert!(
        thread_view.read_with(cx, |thread_view, cx| thread_view.resumed_without_history
            && thread_view.thread.read(cx).entries().is_empty())
    );

    view.update_in(cx, |view, window, cx| view.retry_connection(window, cx));
    cx.run_until_parked();
    assert!(is_connected(&view, cx));
    assert_eq!(
        root_session_of(&view, cx),
        Some(session),
        "the same session"
    );
}
