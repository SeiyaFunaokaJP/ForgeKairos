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

fn lift(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    view.update_in(cx, |view, window, cx| {
        view.restart_with_network(true, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Start Again Without Host Limits");
    cx.run_until_parked();
}

fn restrict(view: &Entity<ConversationView>, cx: &mut VisualTestContext) {
    view.update_in(cx, |view, window, cx| {
        view.restart_with_network(false, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Start Again Restricted");
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

/// Two views of `server`'s agent on one project, sharing one connection store, in the
/// workspace (`in_workspace`) or held by the test alone, which can then drop them.
async fn open_two_views(
    server: Rc<OutsideCapableAgentServer>,
    in_workspace: bool,
    cx: &mut TestAppContext,
) -> (
    Entity<ConversationView>,
    Entity<ConversationView>,
    &mut VisualTestContext,
) {
    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
    let thread_store = cx.update(|_window, cx| cx.new(|cx| ThreadStore::new(cx)));
    let connection_store =
        cx.update(|_window, cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
    let mut views = Vec::new();
    for _ in 0..2 {
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
    let second = views.pop().expect("two views");
    let first = views.pop().expect("two views");
    (first, second, cx)
}

/// ADR-F96: where `allow_unrestricted_network` is off, the network switch of an
/// external agent asks once — the restart's own question also turns the setting
/// on — and starts the agent again once the settings have it, without a second
/// dialog. Declining writes nothing and starts nothing.
#[gpui::test]
async fn test_lifting_an_external_agents_hosts_asks_once_where_it_is_not_allowed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(false, serde_json::json!({}), cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    let session = view.read_with(cx, |view, _| view.root_session_id.clone());
    make_the_settings_folder(&view, cx).await;

    let thread_view = thread_view_of(&view, cx);
    thread_view.update_in(cx, |thread_view, window, cx| {
        thread_view.toggle_external_agent_network(false, window, cx)
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
    assert_eq!(lifted_starts(&server), 0, "declining starts nothing");
    assert!(
        !settings_file_of(&view, cx)
            .await
            .contains("allow_unrestricted_network"),
        "and writes nothing"
    );

    let thread_view = thread_view_of(&view, cx);
    thread_view.update_in(cx, |thread_view, window, cx| {
        thread_view.toggle_external_agent_network(false, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Turn On and Start Again");
    cx.run_until_parked();
    assert_eq!(
        lifted_starts(&server),
        0,
        "nothing starts before the settings have it"
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
    assert_eq!(lifted_starts(&server), 1);
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );
    assert_eq!(
        view.read_with(cx, |view, _| view.root_session_id.clone()),
        session,
        "the conversation is read back"
    );
}

/// ADR-F96: where running outside the sandbox is off, the sandbox switch of an
/// external agent asks once — the start's own question also turns the setting
/// on — and starts the agent outside once the settings have it.
#[gpui::test]
async fn test_letting_an_external_agent_out_asks_once_where_it_is_not_allowed(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_running_outside_sandbox(false, cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    assert_eq!(server.starts(), (1, 0));
    make_the_settings_folder(&view, cx).await;

    let thread_view = thread_view_of(&view, cx);
    thread_view.update_in(cx, |thread_view, window, cx| {
        thread_view.toggle_thread_sandbox(true, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    settings_take_it_in(cx);
    assert!(!cx.has_pending_prompt());
    assert_eq!(server.starts(), (1, 0), "declining starts nothing");
    assert!(
        !settings_file_of(&view, cx)
            .await
            .contains("allow_outside_sandbox"),
        "and writes nothing"
    );

    let thread_view = thread_view_of(&view, cx);
    thread_view.update_in(cx, |thread_view, window, cx| {
        thread_view.toggle_thread_sandbox(true, window, cx)
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
        server.starts(),
        (1, 0),
        "nothing starts before the settings have it"
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
    assert_eq!(server.starts(), (1, 1));
    assert!(view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
}

/// ADR-F96: restricting a thread again closes the connection without the
/// agent's own hosts it leaves, rather than leaving it to the idle disconnect.
/// Threads that lift the restriction share one such connection, which stays
/// for as long as one of them is on it, and is never handed to the others.
#[gpui::test]
async fn test_lifted_threads_share_a_connection_that_closes_when_the_last_leaves(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (first, second, cx) = open_two_views(server.clone(), true, cx).await;
    assert_eq!(server.starts(), (1, 0), "both on the shared connection");

    lift(&first, cx);
    lift(&second, cx);
    assert_eq!(lifted_starts(&server), 1, "one process for both threads");
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    assert_eq!(unrestricted_network_of(&second, cx), Some(id));
    for view in [&first, &second] {
        assert_eq!(
            sandbox_network_of(view, cx),
            Some(AgentSandboxNetwork::AgentHostsLifted)
        );
    }

    restrict(&first, cx);
    assert_eq!(
        sandbox_network_of(&first, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(
        still_running(&first, id, cx),
        "the other thread is still on it"
    );
    assert_eq!(
        sandbox_network_of(&second, cx),
        Some(AgentSandboxNetwork::AgentHostsLifted)
    );

    restrict(&second, cx);
    assert!(
        !still_running(&second, id, cx),
        "closed once no thread is on it"
    );
    assert_eq!(server.starts(), (2, 0), "and nothing else started");

    lift(&first, cx);
    assert_eq!(lifted_starts(&server), 2, "a new one for the next thread");
}

/// ADR-F96: turning `allow_unrestricted_network` off restricts the threads whose
/// external agent runs without its own hosts then, not at their next start: they
/// start again with the agent's hosts, reading the conversation back, and the
/// connection they leave is closed.
#[gpui::test]
async fn test_turning_lifting_off_restricts_the_running_agents_at_once(cx: &mut TestAppContext) {
    init_test(cx);
    allow_lifting_the_network(true, serde_json::json!({}), cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (first, second, cx) = open_two_views(server.clone(), true, cx).await;
    lift(&first, cx);
    lift(&second, cx);
    let id = unrestricted_network_of(&first, cx).expect("lifted");
    let sessions =
        [&first, &second].map(|view| view.read_with(cx, |view, _| view.root_session_id.clone()));

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
            view.read_with(cx, |view, _| view.root_session_id.clone()),
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

/// ADR-F96: starting a thread let out of the sandbox again in it closes the
/// connection outside the sandbox it leaves, rather than leaving that agent to
/// run unsandboxed until the idle disconnect.
#[gpui::test]
async fn test_going_back_into_the_sandbox_closes_the_connection_outside(cx: &mut TestAppContext) {
    init_test(cx);
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    view.update_in(cx, |view, window, cx| {
        view.restart_outside_sandbox(window, cx)
    });
    cx.run_until_parked();
    cx.simulate_prompt_answer(LAUNCH_FOR_THIS_THREAD_LABEL);
    cx.run_until_parked();
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

    view.update_in(cx, |view, window, cx| view.restart_in_sandbox(window, cx));
    cx.run_until_parked();
    cx.simulate_prompt_answer("Start Again in the Sandbox");
    cx.run_until_parked();
    assert!(is_connected(&view, cx));
    assert!(!view.read_with(cx, |view, cx| view.runs_outside_sandbox(cx)));
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
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (first, second, cx) = open_two_views(server.clone(), true, cx).await;
    lift(&first, cx);
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
        "ADR-F99: a report alone replaces nothing"
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

    lift(&second, cx);
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
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (view, cx) = open_outside_capable_view(server.clone(), None, None, cx).await;
    let notice =
        workspace::notifications::NotificationId::unique::<AnswerStoppedByNetworkRestriction>();
    let noticed = |cx: &mut VisualTestContext| {
        view.read_with(cx, |view, cx| {
            view.workspace
                .upgrade()
                .expect("the workspace")
                .read(cx)
                .notification_ids()
                .contains(&notice)
        })
    };

    lift(&view, cx);
    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&view, cx),
        Some(AgentSandboxNetwork::AgentHosts)
    );
    assert!(!noticed(cx), "nothing was cut off");

    allow_lifting_the_network(true, serde_json::json!({}), cx);
    cx.run_until_parked();
    lift(&view, cx);
    let thread_view = thread_view_of(&view, cx);
    message_editor(&view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("Go on", window, cx)
    });
    thread_view.update_in(cx, |thread_view, window, cx| thread_view.send(window, cx));
    cx.run_until_parked();
    assert_eq!(
        thread_view.read_with(cx, |thread_view, cx| thread_view.thread.read(cx).status()),
        ThreadStatus::Generating
    );

    allow_lifting_the_network(false, serde_json::json!({}), cx);
    cx.run_until_parked();
    assert_eq!(
        sandbox_network_of(&view, cx),
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
    let server = Rc::new(OutsideCapableAgentServer::new(None));
    let (first, second, cx) = open_two_views(server.clone(), false, cx).await;
    lift(&first, cx);
    lift(&second, cx);
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
