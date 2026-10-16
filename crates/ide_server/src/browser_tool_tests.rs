//! 内蔵ブラウザーのツール (ADR-F89): 設定が入っていてサンドボックスの外の口にだけ一覧に載り、
//! 呼ぶたびに設定と口を見直し、引数は `browser_control` が解釈し、結果の画像は MCP の image に
//! なる。

use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use browser_control::{
    BrowserCommand, BrowserControl, BrowserOutput, ElementTarget, GlobalBrowserControl, MouseButton,
};
use gpui::{App, EntityId, Task};

use super::*;

#[derive(Default)]
struct FakeBrowser {
    enabled: AtomicBool,
    calls: Mutex<Vec<(EntityId, BrowserCommand)>>,
    reply: Mutex<Option<Result<BrowserOutput, String>>>,
}

impl FakeBrowser {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    fn reply_with(&self, reply: Result<BrowserOutput, String>) {
        *self.reply.lock().expect("the reply is not poisoned") = Some(reply);
    }

    fn take_calls(&self) -> Vec<(EntityId, BrowserCommand)> {
        std::mem::take(&mut *self.calls.lock().expect("the call log is not poisoned"))
    }
}

impl BrowserControl for FakeBrowser {
    fn is_enabled(&self, _cx: &App) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn run(
        &self,
        project: &Entity<Project>,
        command: BrowserCommand,
        _cx: &mut App,
    ) -> Task<anyhow::Result<BrowserOutput>> {
        self.calls
            .lock()
            .expect("the call log is not poisoned")
            .push((project.entity_id(), command));
        let reply = self
            .reply
            .lock()
            .expect("the reply is not poisoned")
            .clone()
            .unwrap_or_else(|| {
                Ok(BrowserOutput {
                    text: "done".to_string(),
                    image_png: None,
                })
            });
        Task::ready(reply.map_err(|message| anyhow::anyhow!(message)))
    }
}

fn install_fake_browser(enabled: bool, cx: &mut TestAppContext) -> Arc<FakeBrowser> {
    let browser = Arc::new(FakeBrowser::default());
    browser.set_enabled(enabled);
    cx.update(|cx| cx.set_global(GlobalBrowserControl(browser.clone())));
    browser
}

fn assist_source() -> AssistSource {
    AssistSource {
        id: "browser-test".into(),
        label: "Agent".into(),
    }
}

fn listed(scope: &tools::ConnectionScope, cx: &mut TestAppContext) -> Vec<String> {
    tool_names(&json!({ "result": cx.update(|cx| tools::list_tools_result(scope, cx)) }))
}

fn browser_names() -> Vec<String> {
    sorted(&browser_control::TOOL_NAMES)
}

fn listed_browser_tools(scope: &tools::ConnectionScope, cx: &mut TestAppContext) -> Vec<String> {
    listed(scope, cx)
        .into_iter()
        .filter(|name| browser_control::TOOL_NAMES.contains(&name.as_str()))
        .collect()
}

/// ADR-F102: `/mcp/zed-browser` for a session whose token is still granted
/// (`live`) or was revoked.
fn browser_session_scope(live: bool) -> tools::ConnectionScope {
    let tokens = transport::ClientTokens::default();
    if live {
        tokens
            .lock()
            .expect("the tokens are not poisoned")
            .insert("session-token".to_string(), "client".to_string());
    }
    tools::ConnectionScope::without_assist(tools::ToolScope::Browser(tools::BrowserSession::new(
        tokens,
        "session-token",
        "client",
    )))
}

/// 口ごとの、ブラウザーのツールを出すかどうか。
fn scopes() -> Vec<(&'static str, tools::ConnectionScope, bool)> {
    let paths = Arc::new(session_paths());
    vec![
        (
            "host WebSocket",
            tools::ConnectionScope::without_assist(tools::ToolScope::Full),
            true,
        ),
        (
            "host /mcp",
            tools::ConnectionScope {
                tools: tools::ToolScope::Full,
                assist: Some(assist_source()),
                browser_separate: false,
            },
            true,
        ),
        (
            "host /mcp/zed",
            tools::ConnectionScope {
                tools: tools::ToolScope::Assist(None),
                assist: Some(assist_source()),
                browser_separate: false,
            },
            true,
        ),
        (
            "an agent's /mcp",
            tools::ConnectionScope {
                tools: tools::ToolScope::Full,
                assist: Some(assist_source()),
                browser_separate: true,
            },
            false,
        ),
        (
            "an agent's /mcp/zed",
            tools::ConnectionScope {
                tools: tools::ToolScope::Assist(None),
                assist: Some(assist_source()),
                browser_separate: true,
            },
            false,
        ),
        ("host /mcp/zed-browser", browser_session_scope(true), true),
        (
            "a revoked session's /mcp/zed-browser",
            browser_session_scope(false),
            false,
        ),
        (
            "sandboxed CLI",
            tools::ConnectionScope::without_assist(tools::ToolScope::Sandboxed(paths.clone())),
            false,
        ),
        (
            "restricted session",
            tools::ConnectionScope::without_assist(tools::ToolScope::SandboxedReadOnly(
                paths.clone(),
            )),
            false,
        ),
        (
            "sandboxed /mcp/zed",
            tools::ConnectionScope {
                tools: tools::ToolScope::Assist(Some(paths)),
                assist: Some(assist_source()),
                browser_separate: false,
            },
            false,
        ),
    ]
}

async fn request_in(
    server: &Entity<IdeServer>,
    scope: tools::ConnectionScope,
    message: Value,
    cx: &mut TestAppContext,
) -> Value {
    let mut async_cx = cx.to_async();
    let raw = crate::handle_message_in(
        server,
        &serde_json::to_string(&message).expect("the request serializes"),
        tools::ConnectionId::next(),
        &Ok(scope),
        &mut async_cx,
    )
    .await
    .expect("the request produced a response");
    serde_json::from_str(&raw).expect("the response is valid JSON")
}

fn call(name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    })
}

fn project_id(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> EntityId {
    workspace.read_with(cx, |workspace, _| workspace.project().entity_id())
}

#[gpui::test]
async fn browser_tools_are_listed_only_where_offered_and_turned_on(cx: &mut TestAppContext) {
    for (label, scope, _) in scopes() {
        assert_eq!(
            listed_browser_tools(&scope, cx),
            Vec::<String>::new(),
            "{label}: no browser has registered itself"
        );
    }

    let browser = install_fake_browser(false, cx);
    for (label, scope, _) in scopes() {
        assert_eq!(
            listed_browser_tools(&scope, cx),
            Vec::<String>::new(),
            "{label}: browser.ai_control is off"
        );
    }

    browser.set_enabled(true);
    for (label, scope, offered) in scopes() {
        let expected = if offered { browser_names() } else { Vec::new() };
        assert_eq!(listed_browser_tools(&scope, cx), expected, "{label}");
    }

    // The other tools of each scope are unchanged by the browser's.
    let full = tools::ConnectionScope::without_assist(tools::ToolScope::Full);
    assert_eq!(
        listed(&full, cx).len(),
        tools::tools().len() + browser_control::TOOL_NAMES.len()
    );
    let names_elsewhere = tools::tools()
        .into_iter()
        .map(|tool| tool.name)
        .chain(crate::ASSIST_TOOL_NAMES.iter().map(|name| name.to_string()))
        .collect::<Vec<_>>();
    for name in browser_control::TOOL_NAMES {
        assert!(
            !names_elsewhere.iter().any(|other| other == name),
            "{name} collides with another tool"
        );
    }

    // ADR-F102: the browser's own endpoint offers nothing else.
    assert_eq!(listed(&browser_session_scope(true), cx), browser_names());

    // The listing carries the contract crate's schemas and descriptions unchanged.
    let result = cx.update(|cx| tools::list_tools_result(&full, cx));
    for definition in browser_control::tool_definitions() {
        let listed = result["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == definition.name))
            .unwrap_or_else(|| panic!("{} is listed", definition.name));
        assert_eq!(listed["inputSchema"], definition.input_schema);
        assert_eq!(listed["description"], json!(definition.description));
    }
}

#[gpui::test]
async fn a_browser_tool_call_reaches_the_workspace_browser_parsed(cx: &mut TestAppContext) {
    let (workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);

    let response = request(&server, call("browser_click", json!({ "ref": "e3" })), cx).await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(
        response["result"]["content"],
        json!([{ "type": "text", "text": "done" }])
    );
    assert_eq!(
        browser.take_calls(),
        vec![(
            project_id(&workspace, cx),
            BrowserCommand::Click {
                tab: None,
                target: ElementTarget::Ref("e3".to_string()),
                button: MouseButton::Left,
                click_count: 1,
                modifiers: Vec::new(),
            }
        )]
    );

    // The assist endpoint runs them as well.
    let assist = tools::ConnectionScope {
        tools: tools::ToolScope::Assist(None),
        assist: Some(assist_source()),
        browser_separate: false,
    };
    let response = request_in(&server, assist, call("browser_tabs", json!({})), cx).await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(
        browser.take_calls(),
        vec![(project_id(&workspace, cx), BrowserCommand::ListTabs)]
    );
}

#[gpui::test]
async fn a_screenshot_becomes_an_mcp_image(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);
    let png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2, 3];
    browser.reply_with(Ok(BrowserOutput {
        text: "Viewport: 1280x720 CSS px".to_string(),
        image_png: Some(png.clone()),
    }));

    let response = request(&server, call("browser_screenshot", json!({})), cx).await;
    assert_eq!(response["result"]["isError"], json!(false), "{response}");
    assert_eq!(
        response["result"]["content"],
        json!([
            { "type": "text", "text": "Viewport: 1280x720 CSS px" },
            {
                "type": "image",
                "data": base64::engine::general_purpose::STANDARD.encode(&png),
                "mimeType": "image/png",
            },
        ])
    );
}

#[gpui::test]
async fn a_result_too_large_for_one_message_is_an_error(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);
    browser.reply_with(Ok(BrowserOutput {
        text: String::new(),
        image_png: Some(vec![0; tools::MAX_BROWSER_RESULT_BYTES]),
    }));

    let response = request(&server, call("browser_screenshot", json!({})), cx).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        first_text(&response).contains("bytes"),
        "the error says the result is too large: {response}"
    );
    assert!(
        response.to_string().len() < 64 * 1024,
        "the oversized image is not sent"
    );
}

/// 上限ちょうどの結果は送り、1 バイトでも超えれば断る。測るのは base64 にした後の大きさで、
/// PNG の生のバイト数が上限の内でも、送る形で超えるなら断る。
#[test]
fn the_result_limit_counts_the_message_as_sent() {
    let overhead = serde_json::to_vec(&vec![tools::text_content("")])
        .expect("the result serializes")
        .len();
    let fitting = BrowserOutput {
        text: "a".repeat(tools::MAX_BROWSER_RESULT_BYTES - overhead),
        image_png: None,
    };
    assert!(tools::browser_output_content("browser_page_text", fitting).is_ok());
    let over = BrowserOutput {
        text: "a".repeat(tools::MAX_BROWSER_RESULT_BYTES - overhead + 1),
        image_png: None,
    };
    let error = tools::browser_output_content("browser_page_text", over)
        .expect_err("one byte over the limit");
    assert!(!error.to_string().contains("  "), "{error}");

    let raw_within_limit = vec![0; tools::MAX_BROWSER_RESULT_BYTES * 4 / 5];
    assert!(
        tools::browser_output_content(
            "browser_screenshot",
            BrowserOutput {
                text: String::new(),
                image_png: Some(raw_within_limit),
            },
        )
        .is_err(),
        "the image is sent as base64, a third larger than the PNG"
    );
}

#[gpui::test]
async fn turning_ai_control_off_refuses_calls_from_an_old_listing(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);
    let listing = request(
        &server,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        cx,
    )
    .await;
    assert!(tool_names(&listing).contains(&"browser_navigate".to_string()));

    browser.set_enabled(false);
    let response = request(
        &server,
        call(
            "browser_navigate",
            json!({ "url": "http://localhost:3000/" }),
        ),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        first_text(&response).contains("browser.ai_control"),
        "the refusal names the setting: {response}"
    );
    assert!(!first_text(&response).contains("  "), "{response}");
    assert_eq!(browser.take_calls(), Vec::new());

    let listing = request(
        &server,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        cx,
    )
    .await;
    assert!(!tool_names(&listing).contains(&"browser_navigate".to_string()));
}

#[gpui::test]
async fn a_sandboxed_scope_cannot_call_a_browser_tool_by_name(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);

    for (label, scope, offered) in scopes() {
        if offered {
            continue;
        }
        let response = request_in(&server, scope, call("browser_tabs", json!({})), cx).await;
        assert_eq!(
            response["error"]["code"],
            json!(protocol::INVALID_PARAMS),
            "{label}: {response}"
        );
        assert!(response.get("result").is_none(), "{label}: {response}");
    }
    assert_eq!(browser.take_calls(), Vec::new());

    // A real sandboxed session, over its own connection.
    let fixture = start_sandbox_fixture(cx).await;
    let mut client = connect(&fixture, cx).await;
    let response = client
        .call_tool("browser_eval", json!({ "expression": "document.cookie" }))
        .await;
    assert_eq!(
        response["error"]["code"],
        json!(protocol::INVALID_PARAMS),
        "{response}"
    );
    let listing = client.request("tools/list", json!({})).await;
    assert!(
        !tool_names(&listing)
            .iter()
            .any(|name| name.starts_with("browser_")),
        "{listing}"
    );
    assert_eq!(browser.take_calls(), Vec::new());
}

#[gpui::test]
async fn bad_arguments_and_browser_failures_come_back_as_tool_errors(cx: &mut TestAppContext) {
    let (_workspace, server) = test_workspace(cx).await;
    let browser = install_fake_browser(true, cx);

    let response = request(&server, call("browser_tabs", json!({ "tab": "1" })), cx).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        first_text(&response).contains("unknown argument `tab`"),
        "{response}"
    );
    let response = request(&server, call("browser_click", json!({})), cx).await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(first_text(&response).contains("`ref`"), "{response}");
    assert_eq!(
        browser.take_calls(),
        Vec::new(),
        "nothing reaches the browser"
    );

    browser.reply_with(Err(
        "example.com is not in the network allowlist".to_string()
    ));
    let response = request(
        &server,
        call("browser_navigate", json!({ "url": "https://example.com/" })),
        cx,
    )
    .await;
    assert_eq!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        first_text(&response).contains("example.com is not in the network allowlist"),
        "{response}"
    );
}

/// ADR-F102: an external agent's registration takes the browser's tools from
/// `/mcp/zed-browser` alone, so that its thread can turn the IDE tools and the
/// browser off apart; a CLI tab's keeps them on its assist endpoint.
#[gpui::test]
async fn an_agent_client_gets_the_browser_on_an_endpoint_of_its_own(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let browser = install_fake_browser(true, cx);
    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    let agent = cx
        .update(|cx| crate::host_agent_assist_client(&fixture.project, "Gemini CLI", cx))
        .expect("an agent registers");
    let tab = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Claude Code", cx))
        .expect("a CLI tab registers");
    cx.run_until_parked();

    let (agent_endpoint, grant, tab_grant) = cx.update(|cx| {
        (
            agent.agent_endpoint(cx).expect("the agent's endpoint"),
            agent
                .grant_browser_session(cx)
                .expect("the browser's endpoint"),
            tab.grant_browser_session(cx),
        )
    });
    assert!(
        grant.endpoint().url.ends_with(crate::BROWSER_MCP_PATH),
        "{}",
        grant.endpoint().url
    );
    assert_ne!(
        grant.endpoint().authorization,
        agent_endpoint.authorization,
        "with a token of the session's own, not the connection's"
    );
    assert!(
        tab_grant.is_none(),
        "a CLI tab has the browser on its assist endpoint"
    );

    let scope_of = |client: &crate::HostAssistClient, endpoint, cx: &mut TestAppContext| {
        let route = transport::Route {
            kind: transport::ConnectionKind::Http,
            endpoint,
            client: Some(client_id_of(client, cx)),
            browser_session: None,
        };
        server
            .read_with(cx, |server, cx| server.connection_scope(&route, cx))
            .expect("the endpoint is open")
    };
    let agent_assist = scope_of(&agent, transport::Endpoint::Assist, cx);
    assert_eq!(
        listed_browser_tools(&agent_assist, cx),
        Vec::<String>::new()
    );
    assert_eq!(
        listed(&agent_assist, cx),
        sorted(&crate::ASSIST_TOOL_NAMES),
        "the IDE tools stay where they were"
    );
    let tab_assist = scope_of(&tab, transport::Endpoint::Assist, cx);
    assert_eq!(listed_browser_tools(&tab_assist, cx), browser_names());

    let gate = server.read_with(cx, |server, _| server.connection_gate());
    let browser_route = transport::Route {
        kind: transport::ConnectionKind::Http,
        endpoint: transport::Endpoint::Browser,
        client: Some(client_id_of(&agent, cx)),
        browser_session: Some(bearer_token(grant.endpoint())),
    };
    assert!(gate.admit(&browser_route).is_ok());
    let agent_browser = server
        .read_with(cx, |server, cx| server.connection_scope(&browser_route, cx))
        .expect("the session's browser endpoint is open");
    assert_eq!(listed(&agent_browser, cx), browser_names());

    // An agent's endpoint for its IDE tools refuses a browser call too.
    let response = request_in(&server, agent_assist, call("browser_tabs", json!({})), cx).await;
    assert_eq!(
        response["error"]["code"],
        json!(protocol::INVALID_PARAMS),
        "{response}"
    );
    assert_eq!(browser.take_calls(), Vec::new());

    // Off with `browser.ai_control`, as the browser's tools were before.
    browser.set_enabled(false);
    assert!(cx.update(|cx| agent.grant_browser_session(cx)).is_none());
    assert_eq!(listed(&agent_browser, cx), Vec::<String>::new());
}

fn bearer_token(endpoint: &crate::McpEndpoint) -> String {
    endpoint
        .authorization
        .strip_prefix("Bearer ")
        .expect("a bearer token")
        .to_string()
}

/// One HTTP request to the host's server, through its transport, as an agent
/// sends it.
async fn post_to_host(
    server: &Entity<IdeServer>,
    token: &str,
    path: &str,
    request: &Value,
    cx: &mut TestAppContext,
) -> String {
    let credentials = server.read_with(cx, |server, _| {
        let running = server.running.as_ref().expect("the server runs");
        server.host_credentials(running.auth_token.clone())
    });
    let (server_end, mut client_end) = memory_stream_pair();
    let connection = cx.foreground_executor().spawn(crate::run_connection(
        server.clone(),
        server_end,
        credentials,
        None,
        cx.to_async(),
    ));
    let body = request.to_string();
    client_end
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\n\
                 Host: 127.0.0.1\r\n\
                 Authorization: Bearer {token}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("writing the request");
    let mut response = String::new();
    client_end
        .read_to_string(&mut response)
        .await
        .expect("reading the response");
    connection.await;
    response
}

fn list_request() -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })
}

/// Whether `token` reaches the browser on `/mcp/zed-browser`, for the list and
/// for a call; anything but both or neither fails the test.
async fn browser_reached_with(
    server: &Entity<IdeServer>,
    browser: &FakeBrowser,
    token: &str,
    cx: &mut TestAppContext,
) -> bool {
    let listing = post_to_host(
        server,
        token,
        transport::BROWSER_MCP_PATH,
        &list_request(),
        cx,
    )
    .await;
    let called = post_to_host(
        server,
        token,
        transport::BROWSER_MCP_PATH,
        &call("browser_tabs", json!({})),
        cx,
    )
    .await;
    let calls = browser.take_calls();
    if listing.starts_with("HTTP/1.1 401") && called.starts_with("HTTP/1.1 401") {
        assert_eq!(calls, Vec::new(), "the browser receives no call");
        return false;
    }
    assert!(listing.starts_with("HTTP/1.1 200"), "{listing:?}");
    assert_eq!(
        tool_names(&http_body(&listing)),
        browser_names(),
        "{listing:?}"
    );
    assert!(called.starts_with("HTTP/1.1 200"), "{called:?}");
    assert_eq!(
        http_body(&called)["result"]["isError"],
        json!(false),
        "{called:?}"
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    true
}

/// ADR-F102: each session given the browser has a token of its own for
/// `/mcp/zed-browser`, and that endpoint takes no other — not the connection's
/// token its other sessions share, nor the server's. Dropping one session's
/// grant (its thread reopened without the browser, closed) shuts that session
/// out, for the list and the call, while the connection's other sessions keep
/// theirs; a session given the browser again gets a fresh token that works.
#[gpui::test]
async fn a_sessions_browser_token_is_revoked_apart_from_the_others(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let browser = install_fake_browser(true, cx);
    let server = cx
        .update(|cx| crate::server_for_project(&fixture.project, cx))
        .expect("the workspace has a server");
    let agent = cx
        .update(|cx| crate::host_agent_assist_client(&fixture.project, "Gemini CLI", cx))
        .expect("an agent registers");
    // Keeps the server running once the agent's connection is gone.
    let _tab = cx
        .update(|cx| crate::host_assist_client(&fixture.project, "Claude Code", cx))
        .expect("a CLI tab registers");
    cx.run_until_parked();
    let (first, second, connection_token) = cx.update(|cx| {
        (
            agent.grant_browser_session(cx).expect("a session's grant"),
            agent.grant_browser_session(cx).expect("another's"),
            bearer_token(&agent.agent_endpoint(cx).expect("the agent's endpoint")),
        )
    });
    let first_token = bearer_token(first.endpoint());
    let second_token = bearer_token(second.endpoint());
    assert_ne!(first_token, second_token);
    assert!(browser_reached_with(&server, &browser, &first_token, cx).await);
    assert!(browser_reached_with(&server, &browser, &second_token, cx).await);

    let server_token = server.read_with(cx, |server, _| {
        server
            .running
            .as_ref()
            .expect("the server runs")
            .auth_token
            .to_string()
    });
    for (label, token) in [
        ("the connection's token", &connection_token),
        ("the server's token", &server_token),
        ("the client's id", &client_id_of(&agent, cx)),
    ] {
        assert!(
            !browser_reached_with(&server, &browser, token, cx).await,
            "{label}"
        );
    }
    let response = post_to_host(
        &server,
        &first_token,
        transport::ASSIST_MCP_PATH,
        &list_request(),
        cx,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "a session's browser token opens nothing else: {response:?}"
    );

    drop(first);
    assert!(
        !browser_reached_with(&server, &browser, &first_token, cx).await,
        "the session reopened without the browser is shut out"
    );
    assert!(
        browser_reached_with(&server, &browser, &second_token, cx).await,
        "the connection's other session keeps its own"
    );

    // A request let in before its session's grant went gets nothing either.
    let route = transport::Route {
        kind: transport::ConnectionKind::Http,
        endpoint: transport::Endpoint::Browser,
        client: Some(client_id_of(&agent, cx)),
        browser_session: Some(second_token.clone()),
    };
    let admitted = server
        .read_with(cx, |server, cx| server.connection_scope(&route, cx))
        .expect("let in while granted");
    let reopened = cx
        .update(|cx| agent.grant_browser_session(cx))
        .expect("the session reopened with the browser on");
    drop(second);
    assert_eq!(listed(&admitted, cx), Vec::<String>::new());
    let response = request_in(&server, admitted, call("browser_tabs", json!({})), cx).await;
    assert!(response["error"].is_object(), "{response}");
    assert_eq!(browser.take_calls(), Vec::new());

    let reopened_token = bearer_token(reopened.endpoint());
    assert!(![first_token.as_str(), second_token.as_str()].contains(&reopened_token.as_str()));
    assert!(browser_reached_with(&server, &browser, &reopened_token, cx).await);

    // The connection going takes every grant of its sessions with it, even one
    // still held.
    drop(agent);
    cx.run_until_parked();
    cx.update(|_| {});
    cx.run_until_parked();
    assert!(!browser_reached_with(&server, &browser, &reopened_token, cx).await);
    drop(reopened);
}

/// ADR-F89 / ADR-F102: a sandbox's session has no `/mcp/zed-browser`, with the
/// AI browser on and with the session's own token.
#[gpui::test]
async fn a_sandboxed_session_refuses_the_browsers_endpoint(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let browser = install_fake_browser(true, cx);
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::BROWSER_MCP_PATH,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    )
    .await;
    assert!(!response.starts_with("HTTP/1.1 200"), "{response:?}");
    let response = post_mcp_to(
        &fixture,
        &fixture.auth_token,
        transport::BROWSER_MCP_PATH,
        &call("browser_tabs", json!({})),
    )
    .await;
    assert!(!response.starts_with("HTTP/1.1 200"), "{response:?}");
    assert_eq!(browser.take_calls(), Vec::new());
}
